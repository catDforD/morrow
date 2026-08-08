use agent_core::{
    AfterToolInput, AgentMiddleware, BeforeToolInput, ContextBlock, FailureMode, GateDecision,
    GateOutput, MiddlewareError, MiddlewareExecutionContext, MiddlewareFuture, ObservationOutput,
    PermissionDecision, PermissionOutput, PermissionRequestInput, ToolResult,
};
use agent_protocol::{MiddlewareAgentScope, MiddlewareSource};
use agent_runtime::{
    BeforePromptInput, CompactionCause, MiddlewareRegistry, PostCompactInput, PreCompactInput,
    RuntimeMiddleware,
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

pub const HOOK_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const HOOK_TRUST_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 10;
pub const MIN_HOOK_TIMEOUT_SECS: u64 = 1;
pub const MAX_HOOK_TIMEOUT_SECS: u64 = 300;
pub const MAX_HOOK_STDOUT_BYTES: usize = 256 * 1024;
pub const MAX_HOOK_STDERR_BYTES: usize = 64 * 1024;
pub const MAX_OPERATION_CONTEXT_BYTES: usize = 64 * 1024;
static TRUST_WRITE_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforePrompt,
    BeforeTool,
    PermissionRequest,
    AfterTool,
    PreCompact,
    PostCompact,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforePrompt => "before_prompt",
            Self::BeforeTool => "before_tool",
            Self::PermissionRequest => "permission_request",
            Self::AfterTool => "after_tool",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
        }
    }

    fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::BeforeTool | Self::PermissionRequest | Self::AfterTool
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureMode {
    #[default]
    Open,
    Closed,
}

impl From<HookFailureMode> for FailureMode {
    fn from(value: HookFailureMode) -> Self {
        match value {
            HookFailureMode::Open => Self::Open,
            HookFailureMode::Closed => Self::Closed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    pub id: String,
    pub event: HookEvent,
    pub command: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub failure_mode: HookFailureMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_scopes: Option<Vec<MiddlewareAgentScope>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookConfigFile {
    schema_version: u32,
    #[serde(default)]
    hooks: Vec<HookDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookConfigSource {
    User,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookDefinitionStatus {
    pub id: String,
    pub event: HookEvent,
    pub command: Vec<String>,
    pub timeout_secs: u64,
    pub failure_mode: HookFailureMode,
    pub tool_names: Option<Vec<String>>,
    pub agent_scopes: Option<Vec<MiddlewareAgentScope>>,
    pub source: HookConfigSource,
    pub trusted: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookSettings {
    pub schema_version: u32,
    pub user_config_path: String,
    pub project_config_path: String,
    pub trust_store_path: String,
    pub project_fingerprint: Option<String>,
    pub project_trusted: bool,
    pub hooks: Vec<HookDefinitionStatus>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
pub struct HookSnapshot {
    registry: Arc<MiddlewareRegistry>,
    settings: HookSettings,
}

impl std::fmt::Debug for HookSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookSnapshot")
            .field("registry", &self.registry)
            .field("settings", &self.settings)
            .finish()
    }
}

impl HookSnapshot {
    pub fn registry(&self) -> Arc<MiddlewareRegistry> {
        self.registry.clone()
    }

    pub fn settings(&self) -> &HookSettings {
        &self.settings
    }
}

#[derive(Debug, Clone)]
pub struct HookManager {
    home_dir: PathBuf,
    workspace_root: PathBuf,
}

impl HookManager {
    pub fn for_workspace(workspace_root: impl Into<PathBuf>) -> Result<Self, HookError> {
        let home_dir = dirs::home_dir().ok_or(HookError::HomeDirNotFound)?;
        Ok(Self::new(home_dir, workspace_root))
    }

    pub fn new(home_dir: impl Into<PathBuf>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home_dir.into(),
            workspace_root: workspace_root.into(),
        }
    }

    pub fn user_config_path(&self) -> PathBuf {
        self.home_dir.join(".morrow").join("hooks.toml")
    }

    pub fn project_config_path(&self) -> PathBuf {
        self.workspace_root.join(".morrow").join("hooks.toml")
    }

    pub fn trust_store_path(&self) -> PathBuf {
        self.home_dir.join(".morrow").join("hook-trust.json")
    }

    pub fn load_snapshot(&self) -> Result<HookSnapshot, HookError> {
        let loaded = self.load_configuration()?;
        let mut registry = MiddlewareRegistry::new();
        let context_budget = Arc::new(AtomicUsize::new(0));
        for hook in &loaded.user_hooks {
            register_hook(
                &mut registry,
                hook.clone(),
                MiddlewareSource::UserCommand,
                context_budget.clone(),
            );
        }
        if loaded.project_trusted {
            for hook in &loaded.project_hooks {
                register_hook(
                    &mut registry,
                    hook.clone(),
                    MiddlewareSource::ProjectCommand,
                    context_budget.clone(),
                );
            }
        }
        Ok(HookSnapshot {
            registry: Arc::new(registry),
            settings: loaded.settings(self),
        })
    }

    pub fn settings(&self) -> Result<HookSettings, HookError> {
        Ok(self.load_configuration()?.settings(self))
    }

    pub fn trust_project(&self) -> Result<HookSettings, HookError> {
        let loaded = self.load_configuration()?;
        let fingerprint = loaded
            .project_fingerprint
            .clone()
            .ok_or(HookError::ProjectConfigNotFound)?;
        let mut trust = self.load_trust_store()?;
        trust.projects.insert(self.workspace_key()?, fingerprint);
        self.save_trust_store(&trust)?;
        self.settings()
    }

    pub fn revoke_project(&self) -> Result<HookSettings, HookError> {
        let mut trust = self.load_trust_store()?;
        trust.projects.remove(&self.workspace_key()?);
        self.save_trust_store(&trust)?;
        self.settings()
    }

    fn load_configuration(&self) -> Result<LoadedHooks, HookError> {
        let user_hooks = load_hook_file(&self.user_config_path())?.unwrap_or_default();
        let project_hooks = load_hook_file(&self.project_config_path())?.unwrap_or_default();
        let project_fingerprint = (!project_hooks.is_empty()
            || self.project_config_path().is_file())
        .then(|| hook_fingerprint(&project_hooks))
        .transpose()?;
        let trust = self.load_trust_store()?;
        let project_trusted = match project_fingerprint.as_ref() {
            Some(fingerprint) => trust.projects.get(&self.workspace_key()?) == Some(fingerprint),
            None => false,
        };
        Ok(LoadedHooks {
            user_hooks,
            project_hooks,
            project_fingerprint,
            project_trusted,
        })
    }

    fn workspace_key(&self) -> Result<String, HookError> {
        let path = fs::canonicalize(&self.workspace_root).map_err(|source| HookError::Io {
            path: self.workspace_root.clone(),
            source,
        })?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn load_trust_store(&self) -> Result<HookTrustStore, HookError> {
        let path = self.trust_store_path();
        if !path.is_file() {
            return Ok(HookTrustStore::default());
        }
        let bytes = fs::read(&path).map_err(|source| HookError::Io {
            path: path.clone(),
            source,
        })?;
        let trust = serde_json::from_slice::<HookTrustStore>(&bytes).map_err(|source| {
            HookError::TrustParse {
                path: path.clone(),
                source,
            }
        })?;
        if trust.schema_version != HOOK_TRUST_SCHEMA_VERSION {
            return Err(HookError::UnsupportedTrustSchema {
                path,
                version: trust.schema_version,
            });
        }
        Ok(trust)
    }

    fn save_trust_store(&self, trust: &HookTrustStore) -> Result<(), HookError> {
        let path = self.trust_store_path();
        let parent = path.parent().expect("trust store has parent");
        fs::create_dir_all(parent).map_err(|source| HookError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let temporary = trust_temporary_path(&path);
        let mut bytes = serde_json::to_vec_pretty(trust).map_err(HookError::TrustSerialize)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| HookError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|source| HookError::Io {
                path: temporary.clone(),
                source,
            })?;
        if let Err(source) = replace_file(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(HookError::Io { path, source });
        }
        sync_parent_directory(parent).map_err(|source| HookError::Io {
            path: parent.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug)]
struct LoadedHooks {
    user_hooks: Vec<HookDefinition>,
    project_hooks: Vec<HookDefinition>,
    project_fingerprint: Option<String>,
    project_trusted: bool,
}

impl LoadedHooks {
    fn settings(&self, manager: &HookManager) -> HookSettings {
        let mut hooks = self
            .user_hooks
            .iter()
            .map(|hook| hook_status(hook, HookConfigSource::User, true))
            .collect::<Vec<_>>();
        hooks.extend(
            self.project_hooks
                .iter()
                .map(|hook| hook_status(hook, HookConfigSource::Project, self.project_trusted)),
        );
        let diagnostics = if self.project_fingerprint.is_some() && !self.project_trusted {
            vec![
                "Project hooks are disabled until this exact configuration is trusted. They can execute repository-controlled commands with the full host environment, including API keys."
                    .to_string(),
            ]
        } else {
            Vec::new()
        };
        HookSettings {
            schema_version: HOOK_CONFIG_SCHEMA_VERSION,
            user_config_path: manager.user_config_path().to_string_lossy().into_owned(),
            project_config_path: manager.project_config_path().to_string_lossy().into_owned(),
            trust_store_path: manager.trust_store_path().to_string_lossy().into_owned(),
            project_fingerprint: self.project_fingerprint.clone(),
            project_trusted: self.project_trusted,
            hooks,
            diagnostics,
        }
    }
}

fn hook_status(
    hook: &HookDefinition,
    source: HookConfigSource,
    trusted: bool,
) -> HookDefinitionStatus {
    HookDefinitionStatus {
        id: hook.id.clone(),
        event: hook.event,
        command: hook.command.clone(),
        timeout_secs: hook.timeout_secs,
        failure_mode: hook.failure_mode,
        tool_names: hook.tool_names.clone(),
        agent_scopes: hook.agent_scopes.clone(),
        source,
        trusted,
        active: trusted,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookTrustStore {
    schema_version: u32,
    #[serde(default)]
    projects: BTreeMap<String, String>,
}

impl Default for HookTrustStore {
    fn default() -> Self {
        Self {
            schema_version: HOOK_TRUST_SCHEMA_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("home directory was not found")]
    HomeDirNotFound,
    #[error("failed to access hook path {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse hook config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("hook config {path} uses unsupported schema v{version}")]
    UnsupportedConfigSchema { path: PathBuf, version: u32 },
    #[error("invalid hook config {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[error("failed to parse hook trust store {path}: {source}")]
    TrustParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("hook trust store {path} uses unsupported schema v{version}")]
    UnsupportedTrustSchema { path: PathBuf, version: u32 },
    #[error("failed to serialize hook trust store: {0}")]
    TrustSerialize(#[source] serde_json::Error),
    #[error("project hook config was not found")]
    ProjectConfigNotFound,
}

fn load_hook_file(path: &Path) -> Result<Option<Vec<HookDefinition>>, HookError> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|source| HookError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config =
        toml::from_str::<HookConfigFile>(&content).map_err(|source| HookError::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
    if config.schema_version != HOOK_CONFIG_SCHEMA_VERSION {
        return Err(HookError::UnsupportedConfigSchema {
            path: path.to_path_buf(),
            version: config.schema_version,
        });
    }
    validate_hooks(path, &config.hooks)?;
    Ok(Some(config.hooks))
}

fn validate_hooks(path: &Path, hooks: &[HookDefinition]) -> Result<(), HookError> {
    let mut ids = HashSet::new();
    for hook in hooks {
        if hook.id.trim().is_empty() {
            return invalid_config(path, "hook id must not be empty");
        }
        if !ids.insert(hook.id.as_str()) {
            return invalid_config(path, format!("duplicate hook id {:?}", hook.id));
        }
        if hook
            .command
            .first()
            .is_none_or(|command| command.trim().is_empty())
        {
            return invalid_config(
                path,
                format!("hook {:?} command must not be empty", hook.id),
            );
        }
        if !(MIN_HOOK_TIMEOUT_SECS..=MAX_HOOK_TIMEOUT_SECS).contains(&hook.timeout_secs) {
            return invalid_config(
                path,
                format!(
                    "hook {:?} timeout_secs must be between {MIN_HOOK_TIMEOUT_SECS} and {MAX_HOOK_TIMEOUT_SECS}",
                    hook.id
                ),
            );
        }
        if hook.tool_names.is_some() && !hook.event.is_tool_event() {
            return invalid_config(
                path,
                format!(
                    "hook {:?} tool_names is only valid for tool events",
                    hook.id
                ),
            );
        }
        validate_exact_list(path, hook, "tool_names", hook.tool_names.as_deref())?;
        if let Some(scopes) = hook.agent_scopes.as_deref()
            && (scopes.is_empty() || scopes.iter().collect::<HashSet<_>>().len() != scopes.len())
        {
            return invalid_config(
                path,
                format!(
                    "hook {:?} agent_scopes must be a non-empty unique list",
                    hook.id
                ),
            );
        }
    }
    Ok(())
}

fn validate_exact_list(
    path: &Path,
    hook: &HookDefinition,
    field: &str,
    values: Option<&[String]>,
) -> Result<(), HookError> {
    if let Some(values) = values
        && (values.is_empty()
            || values.iter().any(|value| value.trim().is_empty())
            || values.iter().collect::<HashSet<_>>().len() != values.len())
    {
        return invalid_config(
            path,
            format!("hook {:?} {field} must be a non-empty unique list", hook.id),
        );
    }
    Ok(())
}

fn invalid_config<T>(path: &Path, message: impl Into<String>) -> Result<T, HookError> {
    Err(HookError::InvalidConfig {
        path: path.to_path_buf(),
        message: message.into(),
    })
}

fn hook_fingerprint(hooks: &[HookDefinition]) -> Result<String, HookError> {
    #[derive(Serialize)]
    struct FingerprintDocument<'a> {
        schema_version: u32,
        hooks: &'a [HookDefinition],
    }
    let bytes = serde_json::to_vec(&FingerprintDocument {
        schema_version: HOOK_CONFIG_SCHEMA_VERSION,
        hooks,
    })
    .map_err(HookError::TrustSerialize)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn default_timeout_secs() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}

fn register_hook(
    registry: &mut MiddlewareRegistry,
    definition: HookDefinition,
    source: MiddlewareSource,
    context_budget: Arc<AtomicUsize>,
) {
    let event = definition.event;
    let failure_mode = definition.failure_mode.into();
    let hook = Arc::new(CommandHook {
        definition,
        source,
        context_budget,
    });
    if event.is_tool_event() {
        registry.register_agent_with_failure_mode(hook, failure_mode);
    } else {
        registry.register_runtime_with_failure_mode(hook, failure_mode);
    }
}

#[derive(Debug)]
struct CommandHook {
    definition: HookDefinition,
    source: MiddlewareSource,
    context_budget: Arc<AtomicUsize>,
}

impl CommandHook {
    fn matches(&self, context: &MiddlewareExecutionContext, tool_name: Option<&str>) -> bool {
        let scope_matches = self
            .definition
            .agent_scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(&context.agent_scope));
        let tool_matches = self.definition.tool_names.as_ref().is_none_or(|names| {
            tool_name.is_some_and(|tool_name| names.iter().any(|name| name == tool_name))
        });
        scope_matches && tool_matches
    }

    async fn invoke(
        &self,
        context: MiddlewareExecutionContext,
        payload: Value,
    ) -> Result<HookCommandResult, MiddlewareError> {
        let invocation_id = context
            .invocation_id
            .clone()
            .ok_or_else(|| MiddlewareError::new("middleware invocation id is missing"))?;
        let request = json!({
            "schema_version": HOOK_CONFIG_SCHEMA_VERSION,
            "invocation_id": invocation_id,
            "event": self.definition.event,
            "context": command_context(&context),
            "payload": payload,
        });
        let input = serde_json::to_vec(&request).map_err(|error| {
            MiddlewareError::new(format!("failed to serialize hook input: {error}"))
        })?;
        let output = run_hook_command(
            &self.definition.command,
            &context.workspace_root,
            self.definition.timeout_secs,
            &context,
            input,
        )
        .await?;
        let response = serde_json::from_slice::<HookCommandResponse>(&output).map_err(|error| {
            MiddlewareError::new(format!("hook stdout is not one valid JSON result: {error}"))
        })?;
        let result = self.validate_response(response)?;
        self.reserve_context(&result.additional_context)?;
        Ok(result)
    }

    fn validate_response(
        &self,
        response: HookCommandResponse,
    ) -> Result<HookCommandResult, MiddlewareError> {
        let allowed = match response.decision {
            HookDecision::Continue => true,
            HookDecision::Approve => self.definition.event == HookEvent::PermissionRequest,
            HookDecision::Deny => matches!(
                self.definition.event,
                HookEvent::BeforePrompt
                    | HookEvent::BeforeTool
                    | HookEvent::PermissionRequest
                    | HookEvent::PreCompact
            ),
        };
        if !allowed {
            return Err(MiddlewareError::new(format!(
                "decision {:?} is invalid for {}",
                response.decision,
                self.definition.event.as_str()
            )));
        }
        let additional_context = response
            .additional_context
            .into_iter()
            .filter_map(|block| {
                let content = block.into_content();
                let content = content.trim().to_string();
                (!content.is_empty()).then_some(ContextBlock::new(content))
            })
            .collect();
        Ok(HookCommandResult {
            decision: response.decision,
            reason: response.reason,
            additional_context,
        })
    }

    fn reserve_context(&self, blocks: &[ContextBlock]) -> Result<(), MiddlewareError> {
        let bytes = blocks
            .iter()
            .map(|block| block.content.len())
            .sum::<usize>();
        self.context_budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_OPERATION_CONTEXT_BYTES)
            })
            .map(|_| ())
            .map_err(|_| {
                MiddlewareError::new(format!(
                    "operation middleware context exceeds {MAX_OPERATION_CONTEXT_BYTES} bytes"
                ))
            })
    }
}

impl AgentMiddleware for CommandHook {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn source(&self) -> MiddlewareSource {
        self.source
    }

    fn before_tool(&self, input: BeforeToolInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::BeforeTool
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(input.context, json!({ "tool_call": input.tool_call }))
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn permission_request(
        &self,
        input: PermissionRequestInput,
    ) -> Option<MiddlewareFuture<PermissionOutput>> {
        if self.definition.event != HookEvent::PermissionRequest
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "tool_call": input.tool_call,
                            "approval_request": input.request,
                        }),
                    )
                    .await?;
                Ok(PermissionOutput {
                    decision: permission_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn after_tool(&self, input: AfterToolInput) -> Option<MiddlewareFuture<ObservationOutput>> {
        if self.definition.event != HookEvent::AfterTool
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "tool_call": input.tool_call,
                            "tool_result": tool_result_json(input.result),
                        }),
                    )
                    .await?;
                Ok(ObservationOutput {
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }
}

impl RuntimeMiddleware for CommandHook {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn source(&self) -> MiddlewareSource {
        self.source
    }

    fn before_prompt(&self, input: BeforePromptInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::BeforePrompt || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(input.context, json!({ "prompt": input.prompt }))
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn pre_compact(&self, input: PreCompactInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::PreCompact || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "cause": compaction_cause(input.cause),
                            "estimated_tokens": input.estimated_tokens,
                            "token_budget": input.token_budget,
                            "current_summary": input.current_summary,
                            "summarized_turns": input.summarized_turns,
                        }),
                    )
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn post_compact(&self, input: PostCompactInput) -> Option<MiddlewareFuture<ObservationOutput>> {
        if self.definition.event != HookEvent::PostCompact || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone_for_future();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "cause": compaction_cause(input.cause),
                            "previous_summary": input.previous_summary,
                            "summary": input.summary,
                            "summarized_turns": input.summarized_turns,
                        }),
                    )
                    .await?;
                Ok(ObservationOutput {
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }
}

impl CommandHook {
    fn clone_for_future(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            source: self.source,
            context_budget: self.context_budget.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookDecision {
    Continue,
    Approve,
    Deny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookCommandResponse {
    decision: HookDecision,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    additional_context: Vec<HookContextValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HookContextValue {
    Text(String),
    Block(HookContextObject),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookContextObject {
    content: String,
}

impl HookContextValue {
    fn into_content(self) -> String {
        match self {
            Self::Text(content) => content,
            Self::Block(block) => block.content,
        }
    }
}

#[derive(Debug)]
struct HookCommandResult {
    decision: HookDecision,
    reason: Option<String>,
    additional_context: Vec<ContextBlock>,
}

fn gate_decision(decision: HookDecision, reason: Option<String>) -> GateDecision {
    match decision {
        HookDecision::Deny => GateDecision::Deny {
            reason: reason.unwrap_or_else(|| "denied by command hook".to_string()),
        },
        HookDecision::Continue | HookDecision::Approve => GateDecision::Continue,
    }
}

fn permission_decision(decision: HookDecision, reason: Option<String>) -> PermissionDecision {
    match decision {
        HookDecision::Continue => PermissionDecision::Continue,
        HookDecision::Approve => PermissionDecision::Approve { reason },
        HookDecision::Deny => PermissionDecision::Deny {
            reason: reason.unwrap_or_else(|| "denied by command hook".to_string()),
        },
    }
}

fn command_context(context: &MiddlewareExecutionContext) -> Value {
    json!({
        "session": context.session,
        "workspace_root": context.workspace_root,
        "turn_index": context.turn_index,
        "operation_id": context.operation_id,
        "turn_id": context.turn_id,
        "model": context.model,
        "permissions": context.permissions,
        "agent_scope": context.agent_scope,
    })
}

fn tool_result_json(result: ToolResult) -> Value {
    json!({
        "ok": result.ok,
        "content": result.content,
        "error": result.error,
        "summary": result.summary,
    })
}

fn compaction_cause(cause: CompactionCause) -> &'static str {
    match cause {
        CompactionCause::Automatic => "automatic",
        CompactionCause::Manual => "manual",
    }
}

async fn run_hook_command(
    argv: &[String],
    workspace_root: &Path,
    timeout_secs: u64,
    context: &MiddlewareExecutionContext,
    input: Vec<u8>,
) -> Result<Vec<u8>, MiddlewareError> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        MiddlewareError::new(format!(
            "failed to start hook command {:?}: {error}",
            argv[0]
        ))
    })?;
    let process_id = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| MiddlewareError::new("hook stderr was not captured"))?;
    let wait = collect_child(&mut child, stdin, stdout, stderr, input);
    let result = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => ChildOutcome::Cancelled,
        result = tokio::time::timeout(Duration::from_secs(timeout_secs), wait) => {
            match result {
                Ok(result) => ChildOutcome::Completed(result),
                Err(_) => ChildOutcome::TimedOut,
            }
        }
    };
    match result {
        ChildOutcome::Completed(result) => validate_child_output(result?),
        ChildOutcome::Cancelled => {
            terminate_child(&mut child, process_id).await;
            Err(MiddlewareError::new("hook command cancelled"))
        }
        ChildOutcome::TimedOut => {
            terminate_child(&mut child, process_id).await;
            Err(MiddlewareError::new(format!(
                "hook command timed out after {timeout_secs} seconds"
            )))
        }
    }
}

enum ChildOutcome {
    Completed(Result<ChildOutput, MiddlewareError>),
    Cancelled,
    TimedOut,
}

struct ChildOutput {
    status: std::process::ExitStatus,
    stdout: LimitedOutput,
    stderr: LimitedOutput,
}

async fn collect_child(
    child: &mut Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    input: Vec<u8>,
) -> Result<ChildOutput, MiddlewareError> {
    let write = async move {
        stdin.write_all(&input).await?;
        stdin.shutdown().await
    };
    let (write, status, stdout, stderr) = tokio::join!(
        write,
        child.wait(),
        read_limited(stdout, MAX_HOOK_STDOUT_BYTES),
        read_limited(stderr, MAX_HOOK_STDERR_BYTES),
    );
    write.map_err(|error| MiddlewareError::new(format!("failed to write hook stdin: {error}")))?;
    Ok(ChildOutput {
        status: status
            .map_err(|error| MiddlewareError::new(format!("failed to wait for hook: {error}")))?,
        stdout: stdout.map_err(|error| {
            MiddlewareError::new(format!("failed to read hook stdout: {error}"))
        })?,
        stderr: stderr.map_err(|error| {
            MiddlewareError::new(format!("failed to read hook stderr: {error}"))
        })?,
    })
}

struct LimitedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<LimitedOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    Ok(LimitedOutput { bytes, exceeded })
}

fn validate_child_output(output: ChildOutput) -> Result<Vec<u8>, MiddlewareError> {
    if output.stdout.exceeded {
        return Err(MiddlewareError::new(format!(
            "hook stdout exceeds {MAX_HOOK_STDOUT_BYTES} bytes"
        )));
    }
    if output.stderr.exceeded {
        return Err(MiddlewareError::new(format!(
            "hook stderr exceeds {MAX_HOOK_STDERR_BYTES} bytes"
        )));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr.bytes);
        let detail = stderr.trim();
        return Err(MiddlewareError::new(if detail.is_empty() {
            format!("hook command exited with {}", output.status)
        } else {
            format!("hook command exited with {}: {detail}", output.status)
        }));
    }
    Ok(output.stdout.bytes)
}

fn trust_temporary_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TRUST_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".hook-trust.json.tmp-{}-{stamp}-{sequence}",
        std::process::id()
    ))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    if !target.exists() {
        return fs::rename(temporary, target);
    }

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    const REPLACEFILE_WRITE_THROUGH: u32 = 0x0000_0001;
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

async fn terminate_child(child: &mut Child, process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_id) = process_id {
        kill_process_group(process_id);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) {
    const SIGKILL: i32 = 9;
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if let Ok(process_id) = i32::try_from(process_id) {
        unsafe {
            let _ = kill(-process_id, SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{
        ModelInvocation, PermissionMode, PermissionProfile, ReasoningLevel, ShellPolicy,
    };
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-hooks-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_config(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create config parent");
        fs::write(path, body).expect("write config");
    }

    fn context(workspace_root: &Path) -> MiddlewareExecutionContext {
        MiddlewareExecutionContext {
            invocation_id: Some("invocation-1".to_string()),
            session: "default".to_string(),
            workspace_root: workspace_root.to_path_buf(),
            turn_index: 0,
            operation_id: None,
            turn_id: None,
            model: ModelInvocation {
                provider_id: "test".to_string(),
                provider_name: "Test".to_string(),
                model_id: "model".to_string(),
                model_name: "Model".to_string(),
                reasoning: ReasoningLevel::Off,
            },
            permissions: PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Deny,
            },
            agent_scope: MiddlewareAgentScope::Main,
            cancellation: agent_core::CancellationToken::new(),
        }
    }

    fn command_hook(event: HookEvent, shell: impl Into<String>) -> CommandHook {
        CommandHook {
            definition: HookDefinition {
                id: "command".to_string(),
                event,
                command: vec!["/bin/sh".to_string(), "-c".to_string(), shell.into()],
                timeout_secs: 10,
                failure_mode: HookFailureMode::Open,
                tool_names: None,
                agent_scopes: None,
            },
            source: MiddlewareSource::UserCommand,
            context_budget: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn config_merges_user_before_project_and_rejects_duplicate_ids_per_file() {
        let home = unique_dir("merge-home");
        let workspace = unique_dir("merge-workspace");
        let manager = HookManager::new(&home, &workspace);
        write_config(
            &manager.user_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"user\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
        );
        write_config(
            &manager.project_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_tool\"\ncommand = [\"true\"]\ntool_names = [\"shell_command\"]\n",
        );
        let settings = manager.settings().expect("settings");
        assert_eq!(settings.hooks[0].id, "user");
        assert_eq!(settings.hooks[1].id, "project");
        assert!(settings.hooks[0].active);
        assert!(!settings.hooks[1].active);

        write_config(
            &manager.user_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"same\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n[[hooks]]\nid = \"same\"\nevent = \"after_tool\"\ncommand = [\"true\"]\n",
        );
        assert!(matches!(
            manager.settings(),
            Err(HookError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn config_validates_exact_matchers_timeouts_and_unknown_fields() {
        let home = unique_dir("validate-home");
        let workspace = unique_dir("validate-workspace");
        let manager = HookManager::new(&home, &workspace);
        for body in [
            "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\ntimeout_secs = 0\n",
            "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\ntool_names = []\n",
            "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_tool\"\ncommand = [\"true\"]\ntool_names = [\"shell_command\", \"shell_command\"]\n",
            "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\nunknown = true\n",
        ] {
            write_config(&manager.user_config_path(), body);
            assert!(manager.settings().is_err(), "config should fail: {body}");
        }
    }

    #[test]
    fn project_fingerprint_tracks_definition_order_but_not_script_contents() {
        let home = unique_dir("fingerprint-home");
        let workspace = unique_dir("fingerprint-workspace");
        let manager = HookManager::new(&home, &workspace);
        let script = workspace.join("hook.sh");
        fs::write(&script, "one").expect("script");
        let first = format!(
            "schema_version = 1\n[[hooks]]\nid = \"a\"\nevent = \"before_prompt\"\ncommand = [{:?}]\n[[hooks]]\nid = \"b\"\nevent = \"pre_compact\"\ncommand = [\"true\"]\n",
            script.to_string_lossy()
        );
        write_config(&manager.project_config_path(), &first);
        let first_fingerprint = manager
            .settings()
            .expect("first")
            .project_fingerprint
            .expect("fingerprint");
        fs::write(&script, "two").expect("change script");
        assert_eq!(
            manager
                .settings()
                .expect("after script change")
                .project_fingerprint
                .as_deref(),
            Some(first_fingerprint.as_str())
        );
        let reordered = first
            .replace("id = \"a\"", "id = \"temporary\"")
            .replace("id = \"b\"", "id = \"a\"")
            .replace("id = \"temporary\"", "id = \"b\"");
        write_config(&manager.project_config_path(), &reordered);
        assert_ne!(
            manager
                .settings()
                .expect("reordered")
                .project_fingerprint
                .as_deref(),
            Some(first_fingerprint.as_str())
        );
    }

    #[test]
    fn trust_and_revoke_are_scoped_to_the_workspace_and_exact_fingerprint() {
        let home = unique_dir("trust-home");
        let workspace = unique_dir("trust-workspace");
        let manager = HookManager::new(&home, &workspace);
        write_config(
            &manager.project_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
        );
        assert!(!manager.settings().expect("untrusted").project_trusted);
        assert!(manager.trust_project().expect("trust").project_trusted);
        let other_host = HookManager::new(unique_dir("other-host-home"), &workspace);
        assert!(
            !other_host
                .settings()
                .expect("other host settings")
                .project_trusted,
            "trust records must remain isolated to the execution host"
        );
        write_config(
            &manager.project_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"false\"]\n",
        );
        assert!(!manager.settings().expect("changed").project_trusted);
        manager.trust_project().expect("retrust");
        assert!(!manager.revoke_project().expect("revoke").project_trusted);
    }

    #[tokio::test]
    async fn untrusted_project_hooks_never_start_and_trusted_hooks_do() {
        let home = unique_dir("untrusted-home");
        let workspace = unique_dir("untrusted-workspace");
        let manager = HookManager::new(&home, &workspace);
        let marker = workspace.join("project-hook-ran");
        let script = workspace.join("project-hook.sh");
        fs::write(
            &script,
            format!(
                "printf ran > '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[]}}'\n",
                marker.display()
            ),
        )
        .expect("write script");
        write_config(
            &manager.project_config_path(),
            &format!(
                "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", {:?}]\n",
                script.to_string_lossy()
            ),
        );

        let snapshot = manager.load_snapshot().expect("untrusted snapshot");
        let run = snapshot
            .registry()
            .runtime()
            .run_before_prompt(BeforePromptInput {
                context: context(&workspace),
                prompt: "hello".to_string(),
            })
            .await;
        assert!(run.events.is_empty());
        assert!(!marker.exists());

        manager.trust_project().expect("trust project");
        let snapshot = manager.load_snapshot().expect("trusted snapshot");
        let run = snapshot
            .registry()
            .runtime()
            .run_before_prompt(BeforePromptInput {
                context: context(&workspace),
                prompt: "hello".to_string(),
            })
            .await;
        assert_eq!(run.events.len(), 2);
        assert_eq!(fs::read_to_string(marker).expect("marker"), "ran");
    }

    #[tokio::test]
    async fn command_hook_failure_modes_default_open_and_allow_closed_override() {
        let home = unique_dir("failure-mode-home");
        let workspace = unique_dir("failure-mode-workspace");
        let manager = HookManager::new(&home, &workspace);
        write_config(
            &manager.user_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"open\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", \"-c\", \"printf invalid\"]\n[[hooks]]\nid = \"closed\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", \"-c\", \"printf invalid\"]\nfailure_mode = \"closed\"\n",
        );

        let run = manager
            .load_snapshot()
            .expect("snapshot")
            .registry()
            .runtime()
            .run_before_prompt(BeforePromptInput {
                context: context(&workspace),
                prompt: "hello".to_string(),
            })
            .await;

        assert!(run.denied());
        assert!(matches!(
            &run.events[1],
            agent_protocol::AgentEvent::MiddlewareFinished(invocation)
                if invocation.outcome == agent_protocol::MiddlewareOutcome::FailedOpen
        ));
        assert!(matches!(
            &run.events[3],
            agent_protocol::AgentEvent::MiddlewareFinished(invocation)
                if invocation.outcome == agent_protocol::MiddlewareOutcome::FailedClosed
        ));
    }

    #[tokio::test]
    async fn loaded_snapshot_is_stable_when_configuration_changes() {
        let home = unique_dir("snapshot-home");
        let workspace = unique_dir("snapshot-workspace");
        let manager = HookManager::new(&home, &workspace);
        let marker = workspace.join("snapshot-marker");
        let old_script = workspace.join("old-hook.sh");
        let new_script = workspace.join("new-hook.sh");
        fs::write(
            &old_script,
            format!(
                "printf old >> '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[{{\"content\":\"old context\"}}]}}'\n",
                marker.display()
            ),
        )
        .expect("old script");
        fs::write(
            &new_script,
            format!(
                "printf new >> '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[\"new context\"]}}'\n",
                marker.display()
            ),
        )
        .expect("new script");
        let config = |script: &Path| {
            format!(
                "schema_version = 1\n[[hooks]]\nid = \"snapshot\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", {:?}]\n",
                script.to_string_lossy()
            )
        };
        write_config(&manager.user_config_path(), &config(&old_script));
        let old_snapshot = manager.load_snapshot().expect("old snapshot");
        write_config(&manager.user_config_path(), &config(&new_script));

        let old_run = old_snapshot
            .registry()
            .runtime()
            .run_before_prompt(BeforePromptInput {
                context: context(&workspace),
                prompt: "hello".to_string(),
            })
            .await;
        assert_eq!(old_run.context[0].content, "old context");
        assert_eq!(fs::read_to_string(&marker).expect("old marker"), "old");

        let new_run = manager
            .load_snapshot()
            .expect("new snapshot")
            .registry()
            .runtime()
            .run_before_prompt(BeforePromptInput {
                context: context(&workspace),
                prompt: "hello".to_string(),
            })
            .await;
        assert_eq!(new_run.context[0].content, "new context");
        assert_eq!(fs::read_to_string(marker).expect("new marker"), "oldnew");
    }

    #[tokio::test]
    async fn command_receives_json_cwd_and_inherited_environment() {
        let workspace = unique_dir("command");
        let output_path = workspace.join("hook-input.json");
        let hook = CommandHook {
            definition: HookDefinition {
                id: "command".to_string(),
                event: HookEvent::BeforePrompt,
                command: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "test -n \"$PATH\" && test \"$PWD\" = {:?} && tee {:?} >/dev/null && printf '%s' '{{\"decision\":\"continue\",\"reason\":null,\"additional_context\":[\"policy\"]}}'",
                        workspace.to_string_lossy(),
                        output_path.to_string_lossy(),
                    ),
                ],
                timeout_secs: 10,
                failure_mode: HookFailureMode::Open,
                tool_names: None,
                agent_scopes: None,
            },
            source: MiddlewareSource::UserCommand,
            context_budget: Arc::new(AtomicUsize::new(0)),
        };
        let result = hook
            .invoke(context(&workspace), json!({ "prompt": "hello" }))
            .await
            .expect("invoke");
        assert_eq!(result.additional_context[0].content, "policy");
        let input: Value =
            serde_json::from_slice(&fs::read(output_path).expect("input")).expect("input JSON");
        assert_eq!(input["schema_version"], 1);
        assert_eq!(input["invocation_id"], "invocation-1");
        assert_eq!(input["event"], "before_prompt");
        assert_eq!(input["payload"]["prompt"], "hello");
    }

    #[tokio::test]
    async fn command_rejects_invalid_json_decisions_and_excess_context() {
        let workspace = unique_dir("invalid-output");
        let invalid_json = command_hook(HookEvent::BeforePrompt, "printf nope");
        let error = invalid_json
            .invoke(context(&workspace), json!({ "prompt": "hello" }))
            .await
            .expect_err("invalid JSON");
        assert!(error.to_string().contains("valid JSON"));

        let invalid_decision = command_hook(
            HookEvent::BeforePrompt,
            "printf '%s' '{\"decision\":\"approve\",\"additional_context\":[]}'",
        );
        let error = invalid_decision
            .invoke(context(&workspace), json!({ "prompt": "hello" }))
            .await
            .expect_err("approve is not valid before prompt");
        assert!(error.to_string().contains("invalid for before_prompt"));

        let budget = command_hook(HookEvent::BeforePrompt, "true");
        let error = budget
            .reserve_context(&[ContextBlock::new(
                "x".repeat(MAX_OPERATION_CONTEXT_BYTES + 1),
            )])
            .expect_err("context over limit");
        assert!(error.to_string().contains("context exceeds"));
    }

    #[tokio::test]
    async fn command_failures_cover_exit_timeout_and_output_limits() {
        let workspace = unique_dir("command-failures");
        let context = context(&workspace);

        let nonzero = run_hook_command(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo detail >&2; exit 7".to_string(),
            ],
            &workspace,
            10,
            &context,
            b"{}".to_vec(),
        )
        .await
        .expect_err("nonzero exit");
        assert!(nonzero.to_string().contains("detail"));

        let started = Instant::now();
        let timeout = run_hook_command(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 30".to_string(),
            ],
            &workspace,
            1,
            &context,
            b"{}".to_vec(),
        )
        .await
        .expect_err("timeout");
        assert!(timeout.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));

        let stdout = run_hook_command(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("head -c {} /dev/zero", MAX_HOOK_STDOUT_BYTES + 1),
            ],
            &workspace,
            10,
            &context,
            b"{}".to_vec(),
        )
        .await
        .expect_err("stdout limit");
        assert!(stdout.to_string().contains("stdout exceeds"));

        let stderr = run_hook_command(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("head -c {} /dev/zero >&2", MAX_HOOK_STDERR_BYTES + 1),
            ],
            &workspace,
            10,
            &context,
            b"{}".to_vec(),
        )
        .await
        .expect_err("stderr limit");
        assert!(stderr.to_string().contains("stderr exceeds"));
    }

    #[test]
    fn matcher_uses_exact_tool_names_and_agent_scopes() {
        let workspace = unique_dir("matcher");
        let mut hook = command_hook(HookEvent::BeforeTool, "true");
        hook.definition.tool_names = Some(vec!["shell_command".to_string()]);
        hook.definition.agent_scopes = Some(vec![MiddlewareAgentScope::Main]);
        let main = context(&workspace);
        assert!(hook.matches(&main, Some("shell_command")));
        assert!(!hook.matches(&main, Some("shell")));
        let mut delegated = main;
        delegated.agent_scope = MiddlewareAgentScope::DelegatedSubagent;
        assert!(!hook.matches(&delegated, Some("shell_command")));
    }

    #[tokio::test]
    async fn cancellation_terminates_a_running_command() {
        let workspace = unique_dir("cancel");
        let child_marker = workspace.join("child-survived");
        let context = context(&workspace);
        let cancellation = context.cancellation.clone();
        let started = Instant::now();
        let argv = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("(sleep 1; touch '{}') & sleep 30", child_marker.display()),
        ];
        let command = run_hook_command(&argv, &workspace, 30, &context, b"{}".to_vec());
        tokio::pin!(command);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => cancellation.cancel(),
            _ = &mut command => panic!("command completed before cancellation"),
        }
        let error = command.await.expect_err("cancelled");
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(
            !child_marker.exists(),
            "cancellation must terminate the Unix process group"
        );
        context.cancellation.cancel();
    }
}
