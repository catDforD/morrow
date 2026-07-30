//! Transport-independent application services for a Morrow workspace.
//!
//! This crate owns session orchestration and the managed settings registries. Front ends send
//! strongly typed [`SessionCommand`] values and subscribe to [`WorkspaceEvent`] values; HTTP,
//! WebSocket, terminal, and desktop adapters do not need to duplicate runtime state machines.

mod commands;
mod mcp_settings;
mod models;
mod secrets;
mod subagent_settings;

#[doc(hidden)]
pub use commands::CommandRegistry;
pub use commands::{
    CommandRegistryError, CommandResponse, CommandSettingsResponse, CommandWriteRequest,
    ResolveCommandRequest, ResolveCommandResponse,
};
pub use mcp_settings::{
    ManagedMcpTransport, McpRegistryError, McpServerResponse, McpServerTestRequest,
    McpServerWriteRequest, McpSettingsResponse,
};
#[doc(hidden)]
pub use mcp_settings::{McpRegistry, config_from_remote_spec, remote_spec_from_config};
#[doc(hidden)]
pub use models::ModelRegistry;
pub use models::{
    DefaultModelRequest, DiscoverModelsRequest, DiscoverModelsResponse, DiscoveredModel,
    FallbackModel, ManagedModel, ModelProviderResponse, ModelRegistryError, ModelSettingsResponse,
    ProviderWriteRequest, ResolvedModel, SessionModelSelectionResponse,
    discover_models as discover_remote_models,
};
#[doc(hidden)]
pub use subagent_settings::SubagentRegistry;
pub use subagent_settings::{
    MAX_SUBAGENT_AVATAR_BYTES, MAX_SUBAGENT_NAME_CHARS, MAX_SUBAGENT_PROFILES,
    MIN_SUBAGENT_PROFILES, SubagentProfileResponse, SubagentProfileWriteRequest,
    SubagentRegistryError, SubagentRoleSettingsResponse, SubagentRoleWriteRequest,
    SubagentSettingsResponse, load_subagent_identities,
};

use agent_config::{ContextConfig, LoadedServerConfig, McpServerConfig};
use agent_model::{ModelError, OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalOrigin, ApprovalRequest,
    ModelSelection, PermissionMode, PermissionProfile, RemoteMcpServerSpec,
    RemoteModelConnectionSpec, RemoteModelSpec, RemoteSubagentMessageSpec, RemoteSubagentRoleSpec,
    RemoteTurnModel, RemoteTurnSpec, Session, SessionDocument, SubagentIdentity,
    SubagentInstanceSnapshot, SubagentRole, SubagentRoleOverride, SubagentRunRecord,
    WorkspaceLocation,
};
use agent_runtime::{
    AgentEventEnvelope, CancellationToken, McpInspection, McpToolCache, Model, RunAgentTurnContext,
    SessionListingEntry, SessionStore, SubagentController, SubagentInstanceDocument,
    SubagentObserver, SubagentRoleRuntime, SubagentSupervisor, TurnEventHandler,
    inspect_mcp_servers, subagent_store_for_session,
};
use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast, oneshot};
use tokio::task::AbortHandle;

pub fn workspace_options_from_loaded_config(
    workspace_root: PathBuf,
    home: &std::path::Path,
    loaded: LoadedServerConfig,
    default_session_name: String,
    permissions: PermissionProfile,
) -> Result<WorkspaceOptions, ModelError> {
    let fallback_model = loaded
        .model
        .map(|model| {
            let model_name = model.config.model.clone();
            let limits = model.config.context_limits();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: model.config.base_url,
                model: model_name.clone(),
                api_key: model.api_key,
                timeout: Duration::from_secs(model.config.timeout_secs),
            })?;
            Ok(FallbackModel {
                provider_name: "默认配置".to_string(),
                model_id: model_name.clone(),
                model_name: model_name.clone(),
                client: Some(client),
                limits,
                reasoning_profile: reasoning_profile(&model_name),
            })
        })
        .transpose()?;
    let workspace_location = WorkspaceLocation::Local {
        path: workspace_root.clone(),
    };
    Ok(WorkspaceOptions {
        fallback_model,
        model_store_path: home.join(".morrow").join("web-models.json"),
        mcp_store_path: home.join(".morrow").join("web-mcp.json"),
        command_store_path: home.join(".morrow").join("commands"),
        subagent_store_path: home.join(".morrow").join("subagents.json"),
        system_prompt: loaded.config.agent.system_prompt,
        context_config: loaded.config.context,
        workspace_root,
        workspace_location,
        config_path: loaded.path,
        config_diagnostics: loaded.diagnostics,
        permissions,
        mcp_servers: loaded.config.mcp_servers,
        default_session_name,
        persistent_settings: true,
    })
}

fn reasoning_profile(model: &str) -> agent_protocol::ReasoningProfile {
    match model {
        "deepseek-v4-flash" | "deepseek-v4-pro" => agent_protocol::ReasoningProfile::Deepseek,
        _ => agent_protocol::ReasoningProfile::None,
    }
}

#[derive(Clone)]
pub struct WorkspaceOptions {
    pub fallback_model: Option<FallbackModel>,
    pub model_store_path: PathBuf,
    pub mcp_store_path: PathBuf,
    pub command_store_path: PathBuf,
    pub subagent_store_path: PathBuf,
    pub system_prompt: String,
    pub context_config: ContextConfig,
    pub workspace_root: PathBuf,
    pub workspace_location: WorkspaceLocation,
    pub config_path: Option<PathBuf>,
    pub config_diagnostics: Vec<String>,
    pub permissions: PermissionProfile,
    pub mcp_servers: Vec<McpServerConfig>,
    pub default_session_name: String,
    /// Load and persist the Morrow-managed settings stores. Remote workspaces use an in-memory
    /// registry populated from the turn specification instead.
    pub persistent_settings: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceErrorKind {
    Validation,
    Conflict,
    NotFound,
    Internal,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct WorkspaceError {
    kind: WorkspaceErrorKind,
    message: String,
}

impl WorkspaceError {
    pub fn kind(&self) -> WorkspaceErrorKind {
        self.kind
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorKind::Validation, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorKind::Conflict, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorKind::NotFound, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorKind::Internal, message)
    }

    fn new(kind: WorkspaceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl From<agent_runtime::RuntimeError> for WorkspaceError {
    fn from(error: agent_runtime::RuntimeError) -> Self {
        Self::internal(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceActivity {
    pub running_turns: usize,
    pub pending_approvals: usize,
}

impl WorkspaceActivity {
    pub fn is_idle(self) -> bool {
        self.running_turns == 0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceStatus {
    pub workspace_root: String,
    pub workspace_location: WorkspaceLocation,
    pub config_path: Option<String>,
    pub permissions: PermissionProfile,
    pub version: &'static str,
    pub model_ready: bool,
    pub model_store_path: String,
    pub mcp_store_path: String,
    pub command_store_path: String,
    pub subagent_store_path: String,
    pub config_diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionEntry {
    pub name: String,
    pub path: String,
    pub turns: usize,
    pub active_messages: usize,
    pub summarized_turns: usize,
    pub has_summary: bool,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionArchive {
    pub name: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningTurnSnapshot {
    pub turn_id: String,
    pub pending_approval: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentTranscriptSnapshot {
    pub instance: SubagentInstanceSnapshot,
    pub model: agent_protocol::ModelInvocation,
    pub permission_ceiling: PermissionProfile,
    pub role_config: SubagentRoleOverride,
    pub session: Session,
    pub runs: Vec<SubagentRunRecord>,
    pub events: Vec<AgentEventEnvelope>,
}

impl SubagentTranscriptSnapshot {
    #[doc(hidden)]
    pub fn from_document(
        document: SubagentInstanceDocument,
        events: Vec<AgentEventEnvelope>,
    ) -> Self {
        Self {
            instance: document.snapshot,
            model: document.model,
            permission_ceiling: document.permission_ceiling,
            role_config: document.role_config,
            session: document.session,
            runs: document.runs,
            events,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    Snapshot {
        session: Session,
        running_turn: Option<RunningTurnSnapshot>,
        permissions: PermissionProfile,
        subagents: Vec<SubagentInstanceSnapshot>,
        approvals: Vec<ApprovalRequest>,
    },
    AgentEvent(Box<AgentEventEnvelope>),
    TurnSaved {
        session: String,
        turn_index: usize,
    },
    TurnRejected {
        request_id: String,
        reason: String,
    },
    ApprovalQueueUpdated {
        approvals: Vec<ApprovalRequest>,
    },
    SubagentTranscript {
        transcript: Box<SubagentTranscriptSnapshot>,
    },
    SubagentDeleted {
        instance_id: String,
    },
    SubagentRejected {
        request_id: String,
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    StartTurn {
        request_id: String,
        prompt: String,
        prompt_resolved: bool,
        permissions: PermissionProfile,
        model_selection: Option<ModelSelection>,
    },
    ApprovalDecision {
        request_id: String,
        approved: bool,
    },
    CancelTurn {
        turn_id: String,
    },
    SpawnSubagent {
        request_id: String,
        role: SubagentRole,
        task: String,
    },
    SendSubagent {
        request_id: String,
        instance_id: String,
        message: String,
        model_selection: Option<ModelSelection>,
    },
    InspectSubagent {
        instance_id: String,
    },
    CancelSubagent {
        instance_id: String,
    },
    DeleteSubagent {
        instance_id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RemoteSubagentCommand {
    SpawnSubagent {
        request_id: String,
        role: SubagentRole,
        task: String,
    },
    SendSubagent {
        request_id: String,
        instance_id: String,
        message: String,
        #[serde(default)]
        model_selection: Option<ModelSelection>,
    },
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error("session event receiver lagged by {0} event(s)")]
    Lagged(u64),
    #[error("session event stream closed")]
    Closed,
}

pub struct SessionSubscription {
    pub snapshot: WorkspaceEvent,
    receiver: broadcast::Receiver<WorkspaceEvent>,
}

impl SessionSubscription {
    pub async fn recv(&mut self) -> Result<WorkspaceEvent, SubscriptionError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(count) => SubscriptionError::Lagged(count),
            broadcast::error::RecvError::Closed => SubscriptionError::Closed,
        })
    }
}

#[derive(Clone)]
pub struct WorkspaceApp {
    inner: Arc<WorkspaceState>,
}

struct WorkspaceState {
    options: WorkspaceOptions,
    model_registry: ModelRegistry,
    mcp_registry: McpRegistry,
    command_registry: CommandRegistry,
    subagent_registry: SubagentRegistry,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    mcp_cache: RwLock<Arc<McpToolCache>>,
    shutting_down: AtomicBool,
}

struct SessionRuntime {
    tx: broadcast::Sender<WorkspaceEvent>,
    running: Option<RunningTurn>,
    approvals: VecDeque<PendingApproval>,
    supervisor: Option<Arc<SubagentSupervisor>>,
    writer_lease: Arc<Semaphore>,
}

struct RunningTurn {
    turn_id: String,
    cancellation: CancellationToken,
    handle: AbortHandle,
}

struct PendingApproval {
    request: ApprovalRequest,
    sender: oneshot::Sender<ApprovalDecision>,
}

impl SessionRuntime {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            running: None,
            approvals: VecDeque::new(),
            supervisor: None,
            writer_lease: Arc::new(Semaphore::new(1)),
        }
    }
}

impl WorkspaceApp {
    pub fn new(options: WorkspaceOptions) -> Result<Self, WorkspaceError> {
        Self::new_with_model_registry_error(options).map_err(workspace_model_error)
    }

    /// Constructs an application while preserving model-store initialization errors for the
    /// legacy server API. Other managed-store initialization failures retain the server's
    /// historical `ModelRegistryError::Validation` mapping.
    #[doc(hidden)]
    pub fn new_with_model_registry_error(
        options: WorkspaceOptions,
    ) -> Result<Self, ModelRegistryError> {
        let model_registry = if options.persistent_settings {
            ModelRegistry::load(
                options.model_store_path.clone(),
                &options.workspace_root,
                options.fallback_model.clone(),
            )
        } else {
            ModelRegistry::in_memory(&options.workspace_root, options.fallback_model.clone())
        }?;
        let mcp_registry = if options.persistent_settings {
            McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
        } else {
            McpRegistry::in_memory(options.mcp_servers.clone())
        }
        .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
        let command_registry = CommandRegistry::new(options.command_store_path.clone());
        let subagent_registry = if options.persistent_settings {
            SubagentRegistry::load(options.subagent_store_path.clone())
        } else {
            Ok(SubagentRegistry::in_memory(
                options.subagent_store_path.clone(),
            ))
        }
        .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
        Ok(Self {
            inner: Arc::new(WorkspaceState {
                options,
                model_registry,
                mcp_registry,
                command_registry,
                subagent_registry,
                sessions: Mutex::new(HashMap::new()),
                mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
                shutting_down: AtomicBool::new(false),
            }),
        })
    }

    pub fn options(&self) -> &WorkspaceOptions {
        &self.inner.options
    }

    pub fn default_permissions(&self) -> PermissionProfile {
        self.inner.options.permissions
    }

    pub async fn status(&self) -> WorkspaceStatus {
        let settings = self.inner.model_registry.settings().await;
        WorkspaceStatus {
            workspace_root: self.inner.options.workspace_root.display().to_string(),
            workspace_location: self.inner.options.workspace_location.clone(),
            config_path: self
                .inner
                .options
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            permissions: self.inner.options.permissions,
            version: env!("CARGO_PKG_VERSION"),
            model_ready: settings.model_ready,
            model_store_path: settings.store_path,
            mcp_store_path: self.inner.options.mcp_store_path.display().to_string(),
            command_store_path: self.inner.command_registry.root().display().to_string(),
            subagent_store_path: self.inner.subagent_registry.path().display().to_string(),
            config_diagnostics: self.inner.options.config_diagnostics.clone(),
        }
    }

    pub async fn model_settings(&self) -> ModelSettingsResponse {
        self.inner.model_registry.settings().await
    }

    pub async fn create_model_provider(
        &self,
        request: ProviderWriteRequest,
    ) -> Result<ModelProviderResponse, WorkspaceError> {
        self.inner
            .model_registry
            .create_provider(request)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn update_model_provider(
        &self,
        provider_id: &str,
        request: ProviderWriteRequest,
    ) -> Result<ModelProviderResponse, WorkspaceError> {
        self.inner
            .model_registry
            .update_provider(provider_id, request)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn delete_model_provider(&self, provider_id: &str) -> Result<(), WorkspaceError> {
        self.inner
            .model_registry
            .delete_provider(provider_id)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn discover_models(
        &self,
        request: DiscoverModelsRequest,
    ) -> Result<DiscoverModelsResponse, WorkspaceError> {
        self.inner
            .model_registry
            .discover(request)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn set_default_model(
        &self,
        selection: ModelSelection,
    ) -> Result<ModelSelection, WorkspaceError> {
        self.inner
            .model_registry
            .set_default(selection)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn session_model_selection(
        &self,
        name: &str,
    ) -> Result<SessionModelSelectionResponse, WorkspaceError> {
        self.session_store(name)?;
        Ok(self.inner.model_registry.session_selection(name).await)
    }

    pub async fn set_session_model_selection(
        &self,
        name: &str,
        selection: ModelSelection,
    ) -> Result<SessionModelSelectionResponse, WorkspaceError> {
        self.session_store(name)?;
        self.inner
            .model_registry
            .set_session_selection(name, selection)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn mcp_settings(&self) -> McpSettingsResponse {
        self.inner.mcp_registry.settings().await
    }

    pub async fn create_mcp_server(
        &self,
        request: McpServerWriteRequest,
    ) -> Result<McpServerResponse, WorkspaceError> {
        let response = self
            .inner
            .mcp_registry
            .create(request)
            .await
            .map_err(workspace_mcp_error)?;
        self.reset_mcp_cache().await;
        Ok(response)
    }

    pub async fn update_mcp_server(
        &self,
        name: &str,
        request: McpServerWriteRequest,
    ) -> Result<McpServerResponse, WorkspaceError> {
        let response = self
            .inner
            .mcp_registry
            .update(name, request)
            .await
            .map_err(workspace_mcp_error)?;
        self.reset_mcp_cache().await;
        Ok(response)
    }

    pub async fn delete_mcp_server(&self, name: &str) -> Result<(), WorkspaceError> {
        self.inner
            .mcp_registry
            .delete(name)
            .await
            .map_err(workspace_mcp_error)?;
        self.reset_mcp_cache().await;
        Ok(())
    }

    pub async fn import_mcp_servers(
        &self,
        value: serde_json::Value,
    ) -> Result<Vec<McpServerResponse>, WorkspaceError> {
        let response = self
            .inner
            .mcp_registry
            .import(value)
            .await
            .map_err(workspace_mcp_error)?;
        self.reset_mcp_cache().await;
        Ok(response)
    }

    pub async fn test_mcp_server(
        &self,
        request: McpServerTestRequest,
    ) -> Result<McpInspection, WorkspaceError> {
        let server = self
            .inner
            .mcp_registry
            .config_for_test(request)
            .await
            .map_err(workspace_mcp_error)?;
        Ok(inspect_mcp_servers(&self.inner.options.workspace_root, &[server]).await)
    }

    pub fn command_settings(&self) -> Result<CommandSettingsResponse, WorkspaceError> {
        self.inner
            .command_registry
            .settings()
            .map_err(workspace_command_error)
    }

    pub async fn create_command(
        &self,
        request: CommandWriteRequest,
    ) -> Result<CommandResponse, WorkspaceError> {
        self.inner
            .command_registry
            .create(request)
            .await
            .map_err(workspace_command_error)
    }

    pub async fn update_command(
        &self,
        name: &str,
        request: CommandWriteRequest,
    ) -> Result<CommandResponse, WorkspaceError> {
        self.inner
            .command_registry
            .update(name, request)
            .await
            .map_err(workspace_command_error)
    }

    pub async fn delete_command(&self, name: &str) -> Result<(), WorkspaceError> {
        self.inner
            .command_registry
            .delete(name)
            .await
            .map_err(workspace_command_error)
    }

    pub fn resolve_command(
        &self,
        request: ResolveCommandRequest,
    ) -> Result<ResolveCommandResponse, WorkspaceError> {
        self.inner
            .command_registry
            .resolve(request)
            .map_err(workspace_command_error)
    }

    pub async fn subagent_settings(&self) -> SubagentSettingsResponse {
        self.inner.subagent_registry.settings().await
    }

    pub async fn update_subagent_role(
        &self,
        role: SubagentRole,
        request: SubagentRoleWriteRequest,
    ) -> Result<SubagentRoleSettingsResponse, WorkspaceError> {
        self.inner
            .subagent_registry
            .update_role(role, request)
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn reset_subagent_roles(
        &self,
    ) -> Result<Vec<SubagentRoleSettingsResponse>, WorkspaceError> {
        self.inner
            .subagent_registry
            .reset_roles()
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn create_subagent(
        &self,
        request: SubagentProfileWriteRequest,
    ) -> Result<SubagentProfileResponse, WorkspaceError> {
        self.inner
            .subagent_registry
            .create(request)
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn update_subagent(
        &self,
        id: &str,
        request: SubagentProfileWriteRequest,
    ) -> Result<SubagentProfileResponse, WorkspaceError> {
        self.inner
            .subagent_registry
            .update(id, request)
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn delete_subagent(&self, id: &str) -> Result<(), WorkspaceError> {
        self.inner
            .subagent_registry
            .delete(id)
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn reset_subagents(&self) -> Result<SubagentSettingsResponse, WorkspaceError> {
        self.inner
            .subagent_registry
            .reset()
            .await
            .map_err(workspace_subagent_error)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionEntry>, WorkspaceError> {
        let store = self.session_store(&self.inner.options.default_session_name)?;
        store
            .list_current_scope_with_archived()
            .map_err(|error| WorkspaceError::internal(error.to_string()))
            .map(|entries| entries.into_iter().map(session_entry).collect())
    }

    pub fn session(&self, name: &str) -> Result<SessionDocument, WorkspaceError> {
        let store = self.session_store(name)?;
        reject_archived_session(&store, name)?;
        store
            .load()
            .map(SessionDocument::new)
            .map_err(|error| WorkspaceError::internal(error.to_string()))
    }

    pub async fn create_session(&self, name: &str) -> Result<SessionDocument, WorkspaceError> {
        if self.session_has_active_work(name).await {
            return Err(WorkspaceError::conflict("session has active agent work"));
        }
        let store = self.session_store(name)?;
        if store.is_archived() {
            return Err(WorkspaceError::conflict(format!(
                "session {name:?} is archived; restore it before creating a session with the same name"
            )));
        }
        match store.load_existing() {
            Ok(_) => {
                return Err(WorkspaceError::conflict(format!(
                    "session {name:?} already exists"
                )));
            }
            Err(agent_runtime::SessionStoreError::SessionNotFound { .. }) => {}
            Err(error) => return Err(WorkspaceError::internal(error.to_string())),
        }
        let session = Session::new();
        store
            .save(&session)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        Ok(SessionDocument::new(session))
    }

    pub async fn reset_session(&self, name: &str) -> Result<SessionDocument, WorkspaceError> {
        if self.session_has_active_work(name).await {
            return Err(WorkspaceError::conflict("session has active agent work"));
        }
        let store = self.session_store(name)?;
        reject_archived_session(&store, name)?;
        let session = Session::new();
        store
            .save(&session)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        let subagents = subagent_store_for_session(&self.inner.options.workspace_root, name)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        subagents
            .reset()
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        if let Some(runtime) = self.inner.sessions.lock().await.get_mut(name) {
            runtime.supervisor = None;
            runtime.approvals.clear();
        }
        Ok(SessionDocument::new(session))
    }

    pub async fn archive_session(&self, name: &str) -> Result<SessionArchive, WorkspaceError> {
        if self.session_has_active_work(name).await {
            return Err(WorkspaceError::conflict("session has active agent work"));
        }
        let store = self.session_store(name)?;
        let subagents = subagent_store_for_session(&self.inner.options.workspace_root, name)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        subagents
            .archive()
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        if let Err(error) = store.archive().map_err(session_mutation_error) {
            let _ = subagents.restore();
            return Err(error);
        }
        self.inner.sessions.lock().await.remove(name);
        Ok(SessionArchive {
            name: name.to_string(),
            archived: true,
        })
    }

    pub async fn restore_session(&self, name: &str) -> Result<SessionArchive, WorkspaceError> {
        if self.session_has_active_work(name).await {
            return Err(WorkspaceError::conflict("session has active agent work"));
        }
        let store = self.session_store(name)?;
        store.restore().map_err(session_mutation_error)?;
        let subagents = subagent_store_for_session(&self.inner.options.workspace_root, name)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        if let Err(error) = subagents.restore() {
            let _ = store.archive();
            return Err(WorkspaceError::internal(error.to_string()));
        }
        Ok(SessionArchive {
            name: name.to_string(),
            archived: false,
        })
    }

    pub async fn subscribe_session(
        &self,
        session_name: &str,
    ) -> Result<SessionSubscription, WorkspaceError> {
        let tx = self.session_sender(session_name).await;
        let receiver = tx.subscribe();
        let snapshot = self.session_snapshot(session_name).await?;
        Ok(SessionSubscription { snapshot, receiver })
    }

    pub async fn session_snapshot(
        &self,
        session_name: &str,
    ) -> Result<WorkspaceEvent, WorkspaceError> {
        let store = self.session_store(session_name)?;
        reject_archived_session(&store, session_name)?;
        let session = store
            .load()
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        let resources = self
            .ensure_session_resources(session_name)
            .await
            .map_err(WorkspaceError::internal)?;
        let subagents = resources.supervisor.snapshots().await;
        let approvals = self.approval_snapshots(session_name).await;
        Ok(WorkspaceEvent::Snapshot {
            session,
            running_turn: self.running_snapshot(session_name).await,
            permissions: self.inner.options.permissions,
            subagents,
            approvals,
        })
    }

    pub async fn send_session_command(
        &self,
        session_name: &str,
        command: SessionCommand,
    ) -> Result<(), WorkspaceError> {
        let tx = self.session_sender(session_name).await;
        self.dispatch_session_command(session_name, command, &tx)
            .await;
        Ok(())
    }

    #[doc(hidden)]
    pub async fn report_session_error(&self, session_name: &str, message: impl ToString) {
        let tx = self.session_sender(session_name).await;
        broadcast_error(&tx, message);
    }

    pub async fn activity(&self) -> WorkspaceActivity {
        let (mut running_turns, pending_approvals, supervisors) = {
            let sessions = self.inner.sessions.lock().await;
            let running_turns = sessions
                .values()
                .filter(|runtime| runtime.running.is_some())
                .count();
            let pending_approvals = sessions
                .values()
                .map(|runtime| runtime.approvals.len())
                .sum();
            let supervisors = sessions
                .values()
                .filter_map(|runtime| runtime.supervisor.clone())
                .collect::<Vec<_>>();
            (running_turns, pending_approvals, supervisors)
        };
        for supervisor in supervisors {
            running_turns += supervisor.active_run_count().await;
        }
        WorkspaceActivity {
            running_turns,
            pending_approvals,
        }
    }

    pub async fn shutdown(&self, cancel_running: bool) {
        self.shutdown_with_timeout(cancel_running, Duration::from_secs(5))
            .await;
    }

    pub async fn shutdown_with_timeout(&self, cancel_running: bool, timeout: Duration) {
        self.begin_shutdown();
        if cancel_running {
            self.cancel_all_turns(timeout).await;
        }
        self.reset_mcp_cache().await;
    }

    #[doc(hidden)]
    pub fn begin_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
    }

    #[doc(hidden)]
    pub fn resume_after_shutdown_rejection(&self) {
        self.inner.shutting_down.store(false, Ordering::Release);
    }

    fn session_store(&self, name: &str) -> Result<SessionStore, WorkspaceError> {
        SessionStore::for_workspace(&self.inner.options.workspace_root, name)
            .map_err(|error| WorkspaceError::validation(error.to_string()))
    }
}

impl WorkspaceApp {
    pub async fn prepare_remote_turn(
        &self,
        session_name: &str,
        request_id: String,
        prompt: String,
        prompt_resolved: bool,
        permission_mode: Option<PermissionMode>,
        model_selection: Option<ModelSelection>,
    ) -> Result<RemoteTurnSpec, WorkspaceError> {
        let prompt = if prompt_resolved {
            prompt
        } else {
            self.resolve_command(ResolveCommandRequest { input: prompt })?
                .prompt
        };
        if prompt.trim().is_empty() {
            return Err(WorkspaceError::validation("prompt must not be empty"));
        }
        let model = self
            .inner
            .model_registry
            .resolve_remote_for_turn(session_name, model_selection)
            .await
            .map_err(workspace_model_error)?;
        let selection = match &model {
            RemoteTurnModel::WorkspaceFallback { selection } => selection.clone(),
            RemoteTurnModel::Managed(spec) => ModelSelection {
                provider_id: spec.invocation.provider_id.clone(),
                model_id: spec.invocation.model_id.clone(),
                reasoning: spec.invocation.reasoning,
            },
        };
        self.inner
            .model_registry
            .set_session_selection(session_name, selection)
            .await
            .map_err(workspace_model_error)?;
        let managed_mcp_servers = self
            .inner
            .mcp_registry
            .managed_servers()
            .await
            .iter()
            .map(remote_spec_from_config)
            .collect();
        let subagent_identities = self.inner.subagent_registry.identities().await;
        let subagent_roles = self
            .remote_subagent_role_specs(session_name, &model)
            .await?;
        Ok(RemoteTurnSpec {
            session: session_name.to_string(),
            request_id,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
            subagent_roles,
        })
    }

    pub async fn start_remote_turn(&self, turn: RemoteTurnSpec) -> Result<(), WorkspaceError> {
        let RemoteTurnSpec {
            session,
            request_id,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
            subagent_roles,
        } = turn;
        let resolved_model = match model {
            RemoteTurnModel::WorkspaceFallback { selection } => self
                .inner
                .model_registry
                .resolve_for_turn(&session, Some(selection))
                .await
                .map_err(workspace_model_error)?,
            RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
        };
        let mut mcp_servers = self.inner.mcp_registry.fallback_servers().to_vec();
        let mut names = mcp_servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<HashSet<_>>();
        for server in managed_mcp_servers {
            if !names.insert(server.name.clone()) {
                return Err(WorkspaceError::validation(format!(
                    "duplicate MCP server name {:?}",
                    server.name
                )));
            }
            mcp_servers.push(config_from_remote_spec(server));
        }
        let tx = self.session_sender(&session).await;
        let (subagent_role_overrides, subagent_role_models) = self
            .resolve_remote_subagent_roles(&session, subagent_roles)
            .await?;
        self.start_turn(
            session.clone(),
            StartTurnRequest {
                request_id,
                prompt,
                prompt_resolved: true,
                permissions: requested_permissions(self.inner.options.permissions, permission_mode),
                model_selection: None,
                resolved_model: Some(resolved_model),
                mcp_servers: Some(mcp_servers),
                subagent_identities: Some(subagent_identities),
                subagent_role_overrides: Some(subagent_role_overrides),
                subagent_role_models: Some(subagent_role_models),
            },
            tx,
        )
        .await;
        Ok(())
    }

    pub async fn prepare_remote_subagent_message(
        &self,
        session_name: &str,
        message: serde_json::Value,
    ) -> Result<RemoteSubagentMessageSpec, WorkspaceError> {
        let resume_selection = match serde_json::from_value::<RemoteSubagentCommand>(
            message.clone(),
        )
        .map_err(|error| {
            WorkspaceError::validation(format!("invalid subagent session message: {error}"))
        })? {
            RemoteSubagentCommand::SendSubagent {
                model_selection, ..
            } => model_selection,
            RemoteSubagentCommand::SpawnSubagent { .. } => None,
        };
        let inherited_model = self
            .inner
            .model_registry
            .resolve_remote_for_turn(session_name, None)
            .await
            .map_err(workspace_model_error)?;
        Ok(RemoteSubagentMessageSpec {
            session: session_name.to_string(),
            message,
            permission_mode: Some(self.inner.options.permissions.mode),
            subagent_identities: self.inner.subagent_registry.identities().await,
            subagent_roles: self
                .remote_subagent_role_specs(session_name, &inherited_model)
                .await?,
            resume_model: match resume_selection {
                Some(selection) => Some(
                    self.inner
                        .model_registry
                        .resolve_remote_for_turn(session_name, Some(selection))
                        .await
                        .map_err(workspace_model_error)?,
                ),
                None => None,
            },
        })
    }

    pub async fn send_remote_subagent_message(
        &self,
        command: RemoteSubagentMessageSpec,
    ) -> Result<(), WorkspaceError> {
        let RemoteSubagentMessageSpec {
            session,
            message,
            permission_mode,
            subagent_identities,
            subagent_roles,
            resume_model,
        } = command;
        let (overrides, models) = self
            .resolve_remote_subagent_roles(&session, subagent_roles)
            .await?;
        let parent_model = models
            .get(&SubagentRole::Explore)
            .cloned()
            .or_else(|| models.values().next().cloned())
            .ok_or_else(|| {
                WorkspaceError::validation("remote subagent runtime contains no models")
            })?;
        let permissions = requested_permissions(self.inner.options.permissions, permission_mode);
        let supervisor = self
            .prepare_subagent_supervisor_with_runtime(
                &session,
                &parent_model,
                permissions,
                &subagent_identities,
                overrides,
                Some(models),
            )
            .await
            .map_err(WorkspaceError::internal)?;
        if let Some(model) = resume_model {
            let resolved = match model {
                RemoteTurnModel::WorkspaceFallback { selection } => self
                    .inner
                    .model_registry
                    .resolve_for_turn(&session, Some(selection))
                    .await
                    .map_err(workspace_model_error)?,
                RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
            };
            let client: Arc<dyn Model> = Arc::new(resolved.client.clone());
            supervisor
                .register_model_runtime(client, resolved.invocation, resolved.limits)
                .await;
        }
        let parsed = serde_json::from_value::<RemoteSubagentCommand>(message).map_err(|error| {
            WorkspaceError::validation(format!("invalid subagent session message: {error}"))
        })?;
        let tx = self.session_sender(&session).await;
        match parsed {
            RemoteSubagentCommand::SpawnSubagent {
                request_id,
                role,
                task,
            } => {
                if let Err(reason) = supervisor.spawn(role, task).await {
                    broadcast_message(&tx, WorkspaceEvent::SubagentRejected { request_id, reason });
                }
            }
            RemoteSubagentCommand::SendSubagent {
                request_id,
                instance_id,
                message,
                ..
            } => {
                if let Err(reason) = supervisor.send(instance_id, message).await {
                    broadcast_message(&tx, WorkspaceEvent::SubagentRejected { request_id, reason });
                }
            }
        }
        Ok(())
    }

    pub async fn prepare_remote_model_discovery(
        &self,
        request: DiscoverModelsRequest,
    ) -> Result<RemoteModelConnectionSpec, WorkspaceError> {
        self.inner
            .model_registry
            .discovery_spec(request)
            .await
            .map_err(workspace_model_error)
    }

    pub async fn prepare_remote_mcp_test(
        &self,
        request: McpServerTestRequest,
    ) -> Result<RemoteMcpServerSpec, WorkspaceError> {
        self.inner
            .mcp_registry
            .config_for_test(request)
            .await
            .map(|server| remote_spec_from_config(&server))
            .map_err(workspace_mcp_error)
    }

    pub async fn inspect_remote_mcp(&self, server: RemoteMcpServerSpec) -> McpInspection {
        inspect_mcp_servers(
            &self.inner.options.workspace_root,
            &[config_from_remote_spec(server)],
        )
        .await
    }

    async fn remote_subagent_role_specs(
        &self,
        session_name: &str,
        inherited_model: &RemoteTurnModel,
    ) -> Result<Vec<RemoteSubagentRoleSpec>, WorkspaceError> {
        let overrides = self.inner.subagent_registry.role_overrides().await;
        let mut roles = Vec::with_capacity(SubagentRole::ALL.len());
        for role in SubagentRole::ALL {
            let role_override = overrides.get(&role).cloned().unwrap_or_default();
            let model = match role_override.model_selection.clone() {
                Some(selection) => self
                    .inner
                    .model_registry
                    .resolve_remote_for_turn(session_name, Some(selection))
                    .await
                    .map_err(|error| {
                        WorkspaceError::validation(format!(
                            "{} subagent model is unavailable: {error}",
                            role.as_str()
                        ))
                    })?,
                None => inherited_model.clone(),
            };
            roles.push(RemoteSubagentRoleSpec {
                role,
                overrides: role_override,
                model,
            });
        }
        Ok(roles)
    }

    async fn resolve_remote_subagent_roles(
        &self,
        session: &str,
        roles: Vec<RemoteSubagentRoleSpec>,
    ) -> Result<
        (
            BTreeMap<SubagentRole, SubagentRoleOverride>,
            BTreeMap<SubagentRole, ResolvedModel>,
        ),
        WorkspaceError,
    > {
        if roles.len() != SubagentRole::ALL.len() {
            return Err(WorkspaceError::validation(
                "remote subagent runtime must contain all four roles",
            ));
        }
        let mut overrides = BTreeMap::new();
        let mut models = BTreeMap::new();
        for role in roles {
            if overrides.insert(role.role, role.overrides).is_some() {
                return Err(WorkspaceError::validation(format!(
                    "duplicate remote subagent role {}",
                    role.role.as_str()
                )));
            }
            let model = match role.model {
                RemoteTurnModel::WorkspaceFallback { selection } => self
                    .inner
                    .model_registry
                    .resolve_for_turn(session, Some(selection))
                    .await
                    .map_err(workspace_model_error)?,
                RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
            };
            models.insert(role.role, model);
        }
        if SubagentRole::ALL
            .into_iter()
            .any(|role| !overrides.contains_key(&role))
        {
            return Err(WorkspaceError::validation(
                "remote subagent runtime is missing a built-in role",
            ));
        }
        Ok((overrides, models))
    }
}

struct AppTurnHandler {
    app: WorkspaceApp,
    session_name: String,
    turn_id: String,
    tx: broadcast::Sender<WorkspaceEvent>,
}

impl TurnEventHandler for AppTurnHandler {
    fn on_event(
        &mut self,
        envelope: &AgentEventEnvelope,
    ) -> Result<(), agent_runtime::RuntimeError> {
        let mut envelope = envelope.clone();
        envelope.origin = AgentEventOrigin::ParentTurn {
            turn_id: Some(self.turn_id.clone()),
            turn_index: envelope.turn_index,
        };
        if let AgentEvent::ApprovalRequested(request) = &mut envelope.event {
            *request = parent_approval_request(request, &self.turn_id);
        }
        broadcast_message(&self.tx, WorkspaceEvent::AgentEvent(Box::new(envelope)));
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, agent_runtime::RuntimeError>> {
        let app = self.app.clone();
        let session_name = self.session_name.clone();
        let turn_id = self.turn_id.clone();
        let request = parent_approval_request(request, &turn_id);
        let tx = self.tx.clone();
        async move {
            {
                let sessions = app.inner.sessions.lock().await;
                let runtime = sessions.get(&session_name).ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("session state disappeared")
                })?;
                let running = runtime.running.as_ref().ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("running turn disappeared")
                })?;
                if running.turn_id != turn_id {
                    return Err(agent_runtime::RuntimeError::event_handler(
                        "running turn changed while waiting for approval",
                    ));
                }
            }
            app.enqueue_approval(&session_name, request, &tx)
                .await
                .map_err(agent_runtime::RuntimeError::event_handler)
        }
        .boxed()
    }
}

fn parent_approval_request(request: &ApprovalRequest, turn_id: &str) -> ApprovalRequest {
    let mut request = request.clone();
    request.origin = ApprovalOrigin::ParentTurn {
        turn_id: Some(turn_id.to_string()),
        tool_call_id: request.id.strip_prefix("approval-").map(str::to_string),
    };
    request
}

struct AppSubagentObserver {
    state: Weak<WorkspaceState>,
    session_name: String,
    tx: broadcast::Sender<WorkspaceEvent>,
}

impl SubagentObserver for AppSubagentObserver {
    fn on_event(&self, event: &AgentEventEnvelope) {
        broadcast_message(
            &self.tx,
            WorkspaceEvent::AgentEvent(Box::new(event.clone())),
        );
    }

    fn resolve_approval(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'static, Result<ApprovalDecision, String>> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let tx = self.tx.clone();
        async move {
            let Some(inner) = state.upgrade() else {
                return Ok(ApprovalDecision::deny(request.id));
            };
            WorkspaceApp { inner }
                .enqueue_approval(&session_name, request, &tx)
                .await
        }
        .boxed()
    }

    fn cancel_approvals(
        &self,
        instance_id: String,
        run_id: Option<String>,
    ) -> BoxFuture<'static, ()> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let tx = self.tx.clone();
        async move {
            let Some(inner) = state.upgrade() else {
                return;
            };
            WorkspaceApp { inner }
                .cancel_matching_approvals(&session_name, &tx, |request| match &request.origin {
                    ApprovalOrigin::SubagentRun {
                        instance_id: pending_instance,
                        run_id: pending_run,
                        ..
                    } => {
                        pending_instance == &instance_id
                            && run_id.as_ref().is_none_or(|run_id| pending_run == run_id)
                    }
                    _ => false,
                })
                .await;
        }
        .boxed()
    }
}

impl WorkspaceApp {
    async fn resolve_approval(
        &self,
        session_name: &str,
        request_id: String,
        approved: bool,
        tx: &broadcast::Sender<WorkspaceEvent>,
    ) {
        let pending = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(runtime) = sessions.get_mut(session_name) else {
                broadcast_error(tx, "session has no pending approval");
                return;
            };
            let Some(front) = runtime.approvals.front() else {
                broadcast_error(tx, "session has no pending approval");
                return;
            };
            if front.request.id != request_id {
                let expected = front.request.id.clone();
                let queued = runtime
                    .approvals
                    .iter()
                    .any(|approval| approval.request.id == request_id);
                broadcast_error(
                    tx,
                    if queued {
                        format!(
                            "approval {request_id} is queued behind current approval {expected}"
                        )
                    } else {
                        format!(
                            "approval decision {request_id} does not match pending approval {expected}"
                        )
                    },
                );
                return;
            }
            runtime
                .approvals
                .pop_front()
                .expect("approval queue front checked")
        };
        let _ = pending.sender.send(if approved {
            ApprovalDecision::approve(request_id)
        } else {
            ApprovalDecision::deny(request_id)
        });
        self.broadcast_approval_queue(session_name, tx).await;
    }

    async fn enqueue_approval(
        &self,
        session_name: &str,
        request: ApprovalRequest,
        tx: &broadcast::Sender<WorkspaceEvent>,
    ) -> Result<ApprovalDecision, String> {
        let request_id = request.id.clone();
        let (sender, receiver) = oneshot::channel();
        {
            let mut sessions = self.inner.sessions.lock().await;
            let runtime = sessions
                .get_mut(session_name)
                .ok_or_else(|| "session state disappeared".to_string())?;
            if runtime
                .approvals
                .iter()
                .any(|approval| approval.request.id == request_id)
            {
                return Err(format!("approval request {request_id:?} is already queued"));
            }
            runtime
                .approvals
                .push_back(PendingApproval { request, sender });
        }
        self.broadcast_approval_queue(session_name, tx).await;
        match receiver.await {
            Ok(decision) => Ok(decision),
            Err(_) => Ok(ApprovalDecision::deny(request_id)),
        }
    }

    async fn cancel_matching_approvals(
        &self,
        session_name: &str,
        tx: &broadcast::Sender<WorkspaceEvent>,
        matches: impl Fn(&ApprovalRequest) -> bool,
    ) {
        let removed = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(runtime) = sessions.get_mut(session_name) else {
                return;
            };
            let mut kept = VecDeque::with_capacity(runtime.approvals.len());
            let mut removed = Vec::new();
            while let Some(approval) = runtime.approvals.pop_front() {
                if matches(&approval.request) {
                    removed.push(approval);
                } else {
                    kept.push_back(approval);
                }
            }
            runtime.approvals = kept;
            removed
        };
        if removed.is_empty() {
            return;
        }
        for pending in removed {
            let _ = pending
                .sender
                .send(ApprovalDecision::deny(pending.request.id));
        }
        self.broadcast_approval_queue(session_name, tx).await;
    }

    async fn broadcast_approval_queue(
        &self,
        session_name: &str,
        tx: &broadcast::Sender<WorkspaceEvent>,
    ) {
        broadcast_message(
            tx,
            WorkspaceEvent::ApprovalQueueUpdated {
                approvals: self.approval_snapshots(session_name).await,
            },
        );
    }

    async fn cancel_turn(
        &self,
        session_name: &str,
        turn_id: String,
        tx: &broadcast::Sender<WorkspaceEvent>,
    ) {
        let cancellation = {
            let sessions = self.inner.sessions.lock().await;
            let Some(runtime) = sessions.get(session_name) else {
                broadcast_error(tx, "session has no running turn");
                return;
            };
            let Some(running) = runtime.running.as_ref() else {
                broadcast_error(tx, "session has no running turn");
                return;
            };
            if running.turn_id != turn_id {
                broadcast_error(tx, format!("turn {turn_id} is not running"));
                return;
            }
            running.cancellation.clone()
        };
        cancellation.cancel();
        self.cancel_matching_approvals(session_name, tx, |request| {
            matches!(
                &request.origin,
                ApprovalOrigin::ParentTurn {
                    turn_id: Some(pending_turn),
                    ..
                } if pending_turn == &turn_id
            )
        })
        .await;
        let app = self.clone();
        let session_name = session_name.to_string();
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let handle = {
                let sessions = app.inner.sessions.lock().await;
                sessions
                    .get(&session_name)
                    .and_then(|runtime| runtime.running.as_ref())
                    .filter(|running| {
                        running.turn_id == turn_id && running.cancellation.is_cancelled()
                    })
                    .map(|running| running.handle.clone())
            };
            if let Some(handle) = handle {
                handle.abort();
                while !handle.is_finished() {
                    tokio::task::yield_now().await;
                }
                app.clear_running_turn(&session_name, &turn_id).await;
                broadcast_error(&tx, format!("turn {turn_id} cancellation timed out"));
            }
        });
    }

    async fn clear_running_turn(&self, session_name: &str, turn_id: &str) {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(runtime) = sessions.get_mut(session_name)
            && runtime
                .running
                .as_ref()
                .is_some_and(|running| running.turn_id == turn_id)
        {
            runtime.running = None;
        }
    }
}

impl WorkspaceApp {
    async fn prepare_subagent_supervisor(
        &self,
        session_name: &str,
        parent_model: &ResolvedModel,
        parent_permissions: PermissionProfile,
        identities: &[SubagentIdentity],
    ) -> Result<Arc<SubagentSupervisor>, String> {
        let overrides = self.inner.subagent_registry.role_overrides().await;
        self.prepare_subagent_supervisor_with_runtime(
            session_name,
            parent_model,
            parent_permissions,
            identities,
            overrides,
            None,
        )
        .await
    }

    async fn prepare_subagent_supervisor_with_runtime(
        &self,
        session_name: &str,
        parent_model: &ResolvedModel,
        parent_permissions: PermissionProfile,
        identities: &[SubagentIdentity],
        overrides: BTreeMap<SubagentRole, SubagentRoleOverride>,
        mut supplied_models: Option<BTreeMap<SubagentRole, ResolvedModel>>,
    ) -> Result<Arc<SubagentSupervisor>, String> {
        let resources = self.ensure_session_resources(session_name).await?;
        let mut roles = BTreeMap::new();
        for role in SubagentRole::ALL {
            let role_config = overrides.get(&role).cloned().unwrap_or_default();
            let resolved = match supplied_models
                .as_mut()
                .and_then(|models| models.remove(&role))
            {
                Some(resolved) => resolved,
                None => match role_config.model_selection.clone() {
                    Some(selection) if selection != parent_model.selection => self
                        .inner
                        .model_registry
                        .resolve_for_turn(session_name, Some(selection))
                        .await
                        .map_err(|error| {
                            format!("{} subagent model is unavailable: {error}", role.as_str())
                        })?,
                    _ => parent_model.clone(),
                },
            };
            let model: Arc<dyn Model> = Arc::new(resolved.client.clone());
            roles.insert(
                role,
                SubagentRoleRuntime {
                    model,
                    invocation: resolved.invocation,
                    limits: resolved.limits,
                    role_config,
                    base_system_prompt: Arc::from(self.inner.options.system_prompt.clone()),
                    parent_permissions,
                },
            );
        }
        resources
            .supervisor
            .update_runtime(roles, identities.to_vec())
            .await;
        for invocation in resources.supervisor.required_models().await {
            let selection = ModelSelection {
                provider_id: invocation.provider_id.clone(),
                model_id: invocation.model_id.clone(),
                reasoning: invocation.reasoning,
            };
            if let Ok(resolved) = self
                .inner
                .model_registry
                .resolve_for_turn(session_name, Some(selection))
                .await
            {
                let model: Arc<dyn Model> = Arc::new(resolved.client.clone());
                resources
                    .supervisor
                    .register_model_runtime(model, resolved.invocation, resolved.limits)
                    .await;
            }
        }
        Ok(resources.supervisor)
    }

    async fn prepare_direct_subagent_supervisor(
        &self,
        session_name: &str,
    ) -> Result<Arc<SubagentSupervisor>, String> {
        let parent_model = self
            .inner
            .model_registry
            .resolve_for_turn(session_name, None)
            .await
            .map_err(|error| error.to_string())?;
        let identities = self.inner.subagent_registry.identities().await;
        self.prepare_subagent_supervisor(
            session_name,
            &parent_model,
            self.inner.options.permissions,
            &identities,
        )
        .await
    }

    pub async fn estimate_context(
        &self,
        session_name: &str,
        prompt: &str,
        permissions: PermissionProfile,
        model_selection: Option<ModelSelection>,
    ) -> Result<agent_runtime::ContextEstimate, WorkspaceError> {
        let prompt = self
            .resolve_command(ResolveCommandRequest {
                input: prompt.to_string(),
            })?
            .prompt;
        let store = self.session_store(session_name)?;
        reject_archived_session(&store, session_name)?;
        let session = store
            .load()
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        let resolved_model = self
            .inner
            .model_registry
            .resolve_for_turn(session_name, model_selection)
            .await
            .map_err(workspace_model_error)?;
        let identities = self.inner.subagent_registry.identities().await;
        let supervisor = self
            .prepare_subagent_supervisor(session_name, &resolved_model, permissions, &identities)
            .await
            .map_err(WorkspaceError::internal)?;
        let mcp_servers = self.inner.mcp_registry.effective_servers().await;
        let mcp_cache = self.inner.mcp_cache.read().await.clone();
        agent_runtime::estimate_context_with_subagent_controller(
            &RunAgentTurnContext {
                client: &resolved_model.client,
                model: &resolved_model.invocation,
                subagent_identities: &identities,
                system_prompt: &self.inner.options.system_prompt,
                context_config: self.inner.options.context_config,
                model_limits: resolved_model.limits,
                workspace_root: &self.inner.options.workspace_root,
                permissions,
                mcp_servers: &mcp_servers,
                mcp_cache: mcp_cache.as_ref(),
                session_name,
                turn_index: session.turns.len(),
            },
            &session,
            &prompt,
            supervisor,
        )
        .await
        .map_err(WorkspaceError::from)
    }

    pub async fn compact_session(
        &self,
        session_name: &str,
        model_selection: Option<ModelSelection>,
    ) -> Result<agent_runtime::CompactionOutcome, WorkspaceError> {
        if self.session_has_active_work(session_name).await {
            return Err(WorkspaceError::conflict("session has active agent work"));
        }
        let store = self.session_store(session_name)?;
        reject_archived_session(&store, session_name)?;
        let mut session = store
            .load()
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        let resolved_model = self
            .inner
            .model_registry
            .resolve_for_turn(session_name, model_selection)
            .await
            .map_err(workspace_model_error)?;
        let outcome = agent_runtime::compact_session(
            &resolved_model.client,
            &mut session,
            self.inner.options.context_config,
        )
        .await
        .map_err(WorkspaceError::from)?;
        if outcome == agent_runtime::CompactionOutcome::Changed {
            store
                .save(&session)
                .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        }
        Ok(outcome)
    }

    pub async fn subagent_transcript(
        &self,
        session_name: &str,
        instance_id: &str,
    ) -> Result<SubagentTranscriptSnapshot, WorkspaceError> {
        let resources = self
            .ensure_session_resources(session_name)
            .await
            .map_err(WorkspaceError::internal)?;
        let document = resources
            .supervisor
            .document(instance_id)
            .await
            .map_err(WorkspaceError::not_found)?;
        let events = resources
            .supervisor
            .events(instance_id)
            .map_err(|error| WorkspaceError::internal(error.to_string()))?;
        Ok(SubagentTranscriptSnapshot::from_document(document, events))
    }

    async fn cancel_all_turns(&self, timeout: Duration) {
        let (handles, supervisors, approvals) = {
            let mut sessions = self.inner.sessions.lock().await;
            let handles = sessions
                .values()
                .filter_map(|runtime| runtime.running.as_ref())
                .map(|running| {
                    running.cancellation.cancel();
                    running.handle.clone()
                })
                .collect::<Vec<_>>();
            let supervisors = sessions
                .values()
                .filter_map(|runtime| runtime.supervisor.clone())
                .collect::<Vec<_>>();
            let approvals = sessions
                .values_mut()
                .flat_map(|runtime| runtime.approvals.drain(..))
                .collect::<Vec<_>>();
            (handles, supervisors, approvals)
        };
        for pending in approvals {
            let _ = pending
                .sender
                .send(ApprovalDecision::deny(pending.request.id));
        }
        for supervisor in &supervisors {
            for snapshot in supervisor.snapshots().await {
                if snapshot.status.is_active() {
                    let _ = supervisor.cancel(snapshot.id).await;
                }
            }
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let parent_active = handles.iter().any(|handle| !handle.is_finished());
            let mut subagent_active = false;
            for supervisor in &supervisors {
                subagent_active |= supervisor.has_active_runs().await;
            }
            if (!parent_active && !subagent_active) || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for handle in handles.iter().filter(|handle| !handle.is_finished()) {
            handle.abort();
        }
        while handles.iter().any(|handle| !handle.is_finished()) {
            tokio::task::yield_now().await;
        }
        let mut sessions = self.inner.sessions.lock().await;
        for runtime in sessions.values_mut() {
            if runtime.running.is_some() {
                runtime.running = None;
            }
        }
    }

    async fn reset_mcp_cache(&self) {
        let previous = {
            let mut current = self.inner.mcp_cache.write().await;
            std::mem::replace(&mut *current, Arc::new(McpToolCache::new()))
        };
        previous.clear().await;
    }
}

fn reject_archived_session(store: &SessionStore, name: &str) -> Result<(), WorkspaceError> {
    if store.is_archived() {
        return Err(WorkspaceError::conflict(format!(
            "session {name:?} is archived; restore it before opening it"
        )));
    }
    Ok(())
}

fn requested_permissions(
    default: PermissionProfile,
    requested_mode: Option<PermissionMode>,
) -> PermissionProfile {
    requested_mode
        .map(PermissionProfile::for_mode)
        .unwrap_or(default)
}

fn resolved_model_from_remote(spec: RemoteModelSpec) -> Result<ResolvedModel, WorkspaceError> {
    if spec.context_window_tokens == 0
        || spec.reserved_output_tokens == 0
        || spec.reserved_output_tokens >= spec.context_window_tokens
    {
        return Err(WorkspaceError::validation(
            "remote model context limits are invalid",
        ));
    }
    if !(1..=600).contains(&spec.timeout_secs) {
        return Err(WorkspaceError::validation(
            "remote model timeout must be between 1 and 600 seconds",
        ));
    }
    let selection = ModelSelection {
        provider_id: spec.invocation.provider_id.clone(),
        model_id: spec.invocation.model_id.clone(),
        reasoning: spec.invocation.reasoning,
    };
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: spec.base_url,
        model: spec.model,
        api_key: spec.api_key,
        timeout: Duration::from_secs(spec.timeout_secs),
    })
    .map_err(|error| WorkspaceError::validation(error.to_string()))?
    .with_request_options(agent_model::OpenAiCompatRequestOptions {
        reasoning_profile: spec.reasoning_profile,
        reasoning: selection.reasoning,
        supports_tools: spec.supports_tools,
    });
    Ok(ResolvedModel {
        selection,
        invocation: spec.invocation,
        client,
        limits: agent_config::ModelContextLimits {
            context_window_tokens: spec.context_window_tokens,
            reserved_output_tokens: spec.reserved_output_tokens,
        },
    })
}

fn session_mutation_error(error: agent_runtime::SessionStoreError) -> WorkspaceError {
    match error {
        agent_runtime::SessionStoreError::SessionNotFound { .. }
        | agent_runtime::SessionStoreError::TargetExists { .. } => {
            WorkspaceError::conflict(error.to_string())
        }
        _ => WorkspaceError::internal(error.to_string()),
    }
}

fn workspace_model_error(error: ModelRegistryError) -> WorkspaceError {
    match error {
        ModelRegistryError::Conflict(_) | ModelRegistryError::SelectionUnavailable(_) => {
            WorkspaceError::conflict(error.to_string())
        }
        ModelRegistryError::Validation(_) | ModelRegistryError::ProviderNotFound(_) => {
            WorkspaceError::validation(error.to_string())
        }
        ModelRegistryError::Model(ModelError::HttpStatus { .. })
        | ModelRegistryError::Model(ModelError::Request(_)) => {
            WorkspaceError::validation(error.to_string())
        }
        _ => WorkspaceError::internal(error.to_string()),
    }
}

fn workspace_mcp_error(error: McpRegistryError) -> WorkspaceError {
    match error {
        McpRegistryError::Validation(_) => WorkspaceError::validation(error.to_string()),
        McpRegistryError::Conflict(_) => WorkspaceError::conflict(error.to_string()),
        McpRegistryError::NotFound(_) => WorkspaceError::not_found(error.to_string()),
        _ => WorkspaceError::internal(error.to_string()),
    }
}

fn workspace_command_error(error: CommandRegistryError) -> WorkspaceError {
    match error {
        CommandRegistryError::Validation(_) => WorkspaceError::validation(error.to_string()),
        CommandRegistryError::Conflict(_) => WorkspaceError::conflict(error.to_string()),
        CommandRegistryError::NotFound(_) => WorkspaceError::not_found(error.to_string()),
        _ => WorkspaceError::internal(error.to_string()),
    }
}

fn workspace_subagent_error(error: SubagentRegistryError) -> WorkspaceError {
    match error {
        SubagentRegistryError::Validation(_) => WorkspaceError::validation(error.to_string()),
        SubagentRegistryError::Conflict(_) => WorkspaceError::conflict(error.to_string()),
        SubagentRegistryError::NotFound(_) => WorkspaceError::not_found(error.to_string()),
        _ => WorkspaceError::internal(error.to_string()),
    }
}

fn session_entry(entry: SessionListingEntry) -> SessionEntry {
    let session = entry.session;
    SessionEntry {
        name: session.name,
        path: session.path.display().to_string(),
        turns: session.turns,
        active_messages: session.active_messages,
        summarized_turns: session.summarized_turns,
        has_summary: session.has_summary,
        archived: entry.archived,
    }
}

fn broadcast_message(tx: &broadcast::Sender<WorkspaceEvent>, message: WorkspaceEvent) {
    let _ = tx.send(message);
}

fn broadcast_error(tx: &broadcast::Sender<WorkspaceEvent>, message: impl ToString) {
    broadcast_message(
        tx,
        WorkspaceEvent::Error {
            message: message.to_string(),
        },
    );
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use agent_protocol::ShellPolicy;
    use std::fs;

    fn test_app(name: &str) -> WorkspaceApp {
        let root = std::env::temp_dir().join(format!(
            "morrow-app-{name}-{}-{}",
            agent_runtime::timestamp_ms(),
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create workspace");
        WorkspaceApp::new(WorkspaceOptions {
            fallback_model: None,
            model_store_path: root.join("web-models.json"),
            mcp_store_path: root.join("web-mcp.json"),
            command_store_path: root.join("commands"),
            subagent_store_path: root.join("subagents.json"),
            system_prompt: "system".to_string(),
            context_config: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 2,
            },
            workspace_root: root.clone(),
            workspace_location: WorkspaceLocation::Local { path: root },
            config_path: None,
            config_diagnostics: Vec::new(),
            permissions: PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Deny,
            },
            mcp_servers: Vec::new(),
            default_session_name: "default".to_string(),
            persistent_settings: false,
        })
        .expect("app")
    }

    async fn wait_for_approval_count(app: &WorkspaceApp, session: &str, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if app.approval_snapshots(session).await.len() == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approval queue reached expected length");
    }

    async fn wait_for_idle_snapshot(
        receiver: &mut tokio::sync::broadcast::Receiver<WorkspaceEvent>,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let WorkspaceEvent::Snapshot { running_turn, .. } =
                    receiver.recv().await.expect("workspace event")
                {
                    assert!(running_turn.is_none());
                    break;
                }
            }
        })
        .await
        .expect("terminal snapshot broadcast");
    }

    async fn reserve_turn(app: &WorkspaceApp, turn_id: &str, worker: &tokio::task::JoinHandle<()>) {
        let mut sessions = app.inner.sessions.lock().await;
        let runtime = sessions.get_mut("default").expect("session runtime");
        runtime.running = Some(RunningTurn {
            turn_id: turn_id.to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
    }

    #[tokio::test]
    async fn approval_queue_is_fifo_across_parent_and_subagent_sources() {
        let app = test_app("approval-fifo");
        let tx = app.session_sender("default").await;
        let mut rx = tx.subscribe();
        let parent = ApprovalRequest::shell_command(
            "approval-parent",
            "cargo test",
            ".",
            30,
            "verify parent changes",
        )
        .with_origin(ApprovalOrigin::ParentTurn {
            turn_id: Some("turn-1".to_string()),
            tool_call_id: Some("parent-call".to_string()),
        });
        let child = ApprovalRequest::shell_command(
            "approval-child",
            "cargo clippy",
            ".",
            30,
            "verify child changes",
        )
        .with_origin(ApprovalOrigin::SubagentRun {
            instance_id: "subagent-1".to_string(),
            run_id: "subrun-1".to_string(),
            role: SubagentRole::Reviewer,
            identity_id: Some("builtin-01".to_string()),
            identity_name: Some("Reviewer".to_string()),
            tool_call_id: Some("child-call".to_string()),
        });
        let first = tokio::spawn({
            let app = app.clone();
            let tx = tx.clone();
            async move { app.enqueue_approval("default", parent, &tx).await }
        });
        wait_for_approval_count(&app, "default", 1).await;
        let second = tokio::spawn({
            let app = app.clone();
            let tx = tx.clone();
            async move { app.enqueue_approval("default", child, &tx).await }
        });
        wait_for_approval_count(&app, "default", 2).await;
        let snapshot = app.session_snapshot("default").await.expect("snapshot");
        let WorkspaceEvent::Snapshot { approvals, .. } = snapshot else {
            panic!("snapshot expected");
        };
        assert_eq!(
            approvals
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["approval-parent", "approval-child"]
        );

        app.resolve_approval("default", "approval-child".to_string(), true, &tx)
            .await;
        let queued_error = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let WorkspaceEvent::Error { message } = rx.recv().await.expect("event")
                    && message.contains("queued behind")
                {
                    break message;
                }
            }
        })
        .await
        .expect("queued decision rejected");
        assert!(queued_error.contains("approval-parent"));
        app.resolve_approval("default", "approval-parent".to_string(), true, &tx)
            .await;
        assert!(first.await.expect("task").expect("decision").approved);
        app.resolve_approval("default", "approval-child".to_string(), false, &tx)
            .await;
        assert!(!second.await.expect("task").expect("decision").approved);
    }

    #[tokio::test]
    async fn cancelling_subagent_approvals_denies_only_matching_run() {
        let app = test_app("approval-cancel");
        let tx = app.session_sender("default").await;
        let child = ApprovalRequest::shell_command(
            "approval-child-cancel",
            "pwd",
            ".",
            30,
            "child request",
        )
        .with_origin(ApprovalOrigin::SubagentRun {
            instance_id: "subagent-cancel".to_string(),
            run_id: "subrun-cancel".to_string(),
            role: SubagentRole::Worker,
            identity_id: None,
            identity_name: None,
            tool_call_id: None,
        });
        let pending = tokio::spawn({
            let app = app.clone();
            let tx = tx.clone();
            async move { app.enqueue_approval("default", child, &tx).await }
        });
        wait_for_approval_count(&app, "default", 1).await;
        app.cancel_matching_approvals("default", &tx, |request| {
            matches!(
                &request.origin,
                ApprovalOrigin::SubagentRun { instance_id, run_id, .. }
                    if instance_id == "subagent-cancel" && run_id == "subrun-cancel"
            )
        })
        .await;
        let decision = pending.await.expect("task").expect("decision");
        assert!(!decision.approved);
        assert_eq!(decision.request_id, "approval-child-cancel");
        assert!(app.approval_snapshots("default").await.is_empty());
    }

    #[tokio::test]
    async fn cancelling_turn_keeps_session_reserved_until_worker_cleanup() {
        let app = test_app("turn-cancel");
        let tx = app.session_sender("default").await;
        let worker = tokio::spawn(std::future::pending::<()>());
        {
            let mut sessions = app.inner.sessions.lock().await;
            let runtime = sessions.get_mut("default").expect("runtime");
            runtime.running = Some(RunningTurn {
                turn_id: "turn-1".to_string(),
                cancellation: CancellationToken::new(),
                handle: worker.abort_handle(),
            });
        }
        app.cancel_turn("default", "turn-1".to_string(), &tx).await;
        let sessions = app.inner.sessions.lock().await;
        let running = sessions["default"].running.as_ref().expect("reserved");
        assert!(running.cancellation.is_cancelled());
        assert!(!worker.is_finished());
        drop(sessions);
        app.clear_running_turn("default", "turn-1").await;
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn worker_cleanup_broadcasts_idle_snapshot_after_exit_panic_and_timeout_abort() {
        let app = test_app("worker-terminal-snapshot");
        let tx = app.session_sender("default").await;
        let mut receiver = tx.subscribe();

        let completed = tokio::spawn(async {});
        reserve_turn(&app, "turn-completed", &completed).await;
        app.supervise_turn_worker(
            "default".to_string(),
            "turn-completed".to_string(),
            tx.clone(),
            completed,
        )
        .await;
        wait_for_idle_snapshot(&mut receiver).await;

        let panicked = tokio::spawn(async { panic!("test worker panic") });
        reserve_turn(&app, "turn-panicked", &panicked).await;
        app.supervise_turn_worker(
            "default".to_string(),
            "turn-panicked".to_string(),
            tx.clone(),
            panicked,
        )
        .await;
        wait_for_idle_snapshot(&mut receiver).await;

        let timeout_aborted = tokio::spawn(std::future::pending::<()>());
        reserve_turn(&app, "turn-timeout", &timeout_aborted).await;
        timeout_aborted.abort();
        app.supervise_turn_worker(
            "default".to_string(),
            "turn-timeout".to_string(),
            tx,
            timeout_aborted,
        )
        .await;
        wait_for_idle_snapshot(&mut receiver).await;
    }
}

#[derive(Clone)]
struct SessionResources {
    tx: broadcast::Sender<WorkspaceEvent>,
    supervisor: Arc<SubagentSupervisor>,
}

impl WorkspaceApp {
    async fn dispatch_session_command(
        &self,
        session_name: &str,
        command: SessionCommand,
        tx: &broadcast::Sender<WorkspaceEvent>,
    ) {
        match command {
            SessionCommand::StartTurn {
                request_id,
                prompt,
                prompt_resolved,
                permissions,
                model_selection,
            } => {
                self.start_turn(
                    session_name.to_string(),
                    StartTurnRequest {
                        request_id,
                        prompt,
                        prompt_resolved,
                        permissions,
                        model_selection,
                        resolved_model: None,
                        mcp_servers: None,
                        subagent_identities: None,
                        subagent_role_overrides: None,
                        subagent_role_models: None,
                    },
                    tx.clone(),
                )
                .await;
            }
            SessionCommand::ApprovalDecision {
                request_id,
                approved,
            } => {
                self.resolve_approval(session_name, request_id, approved, tx)
                    .await;
            }
            SessionCommand::CancelTurn { turn_id } => {
                self.cancel_turn(session_name, turn_id, tx).await;
            }
            SessionCommand::SpawnSubagent {
                request_id,
                role,
                task,
            } => {
                let result = async {
                    let supervisor = self
                        .prepare_direct_subagent_supervisor(session_name)
                        .await?;
                    supervisor.spawn(role, task).await
                }
                .await;
                if let Err(reason) = result {
                    broadcast_message(tx, WorkspaceEvent::SubagentRejected { request_id, reason });
                }
            }
            SessionCommand::SendSubagent {
                request_id,
                instance_id,
                message,
                model_selection,
            } => {
                let result = async {
                    let supervisor = self
                        .prepare_direct_subagent_supervisor(session_name)
                        .await?;
                    if let Some(selection) = model_selection {
                        let resolved = self
                            .inner
                            .model_registry
                            .resolve_for_turn(session_name, Some(selection))
                            .await
                            .map_err(|error| error.to_string())?;
                        let model: Arc<dyn Model> = Arc::new(resolved.client.clone());
                        supervisor
                            .register_model_runtime(model, resolved.invocation, resolved.limits)
                            .await;
                    }
                    supervisor.send(instance_id, message).await
                }
                .await;
                if let Err(reason) = result {
                    broadcast_message(tx, WorkspaceEvent::SubagentRejected { request_id, reason });
                }
            }
            SessionCommand::InspectSubagent { instance_id } => {
                let result = async {
                    let resources = self.ensure_session_resources(session_name).await?;
                    let document = resources.supervisor.document(&instance_id).await?;
                    let events = resources
                        .supervisor
                        .events(&instance_id)
                        .map_err(|error| error.to_string())?;
                    Ok::<_, String>(SubagentTranscriptSnapshot::from_document(document, events))
                }
                .await;
                match result {
                    Ok(transcript) => broadcast_message(
                        tx,
                        WorkspaceEvent::SubagentTranscript {
                            transcript: Box::new(transcript),
                        },
                    ),
                    Err(error) => broadcast_error(tx, error),
                }
            }
            SessionCommand::CancelSubagent { instance_id } => {
                let result = async {
                    let resources = self.ensure_session_resources(session_name).await?;
                    resources.supervisor.cancel(instance_id).await
                }
                .await;
                if let Err(error) = result {
                    broadcast_error(tx, error);
                }
            }
            SessionCommand::DeleteSubagent { instance_id } => {
                let result = async {
                    let resources = self.ensure_session_resources(session_name).await?;
                    resources.supervisor.delete(&instance_id).await
                }
                .await;
                match result {
                    Ok(()) => {
                        broadcast_message(tx, WorkspaceEvent::SubagentDeleted { instance_id })
                    }
                    Err(error) => broadcast_error(tx, error),
                }
            }
        }
    }

    async fn ensure_session_resources(
        &self,
        session_name: &str,
    ) -> Result<SessionResources, String> {
        let identities = self.inner.subagent_registry.identities().await;
        let mut sessions = self.inner.sessions.lock().await;
        let runtime = sessions
            .entry(session_name.to_string())
            .or_insert_with(SessionRuntime::new);
        if runtime.supervisor.is_none() {
            let observer = Arc::new(AppSubagentObserver {
                state: Arc::downgrade(&self.inner),
                session_name: session_name.to_string(),
                tx: runtime.tx.clone(),
            });
            let supervisor = SubagentSupervisor::new_with_writer_lease(
                self.inner.options.workspace_root.clone(),
                session_name.to_string(),
                self.inner.options.context_config,
                BTreeMap::new(),
                identities,
                observer,
                runtime.writer_lease.clone(),
            )
            .map_err(|error| error.to_string())?;
            runtime.supervisor = Some(Arc::new(supervisor));
        }
        Ok(SessionResources {
            tx: runtime.tx.clone(),
            supervisor: runtime
                .supervisor
                .as_ref()
                .expect("subagent supervisor initialized")
                .clone(),
        })
    }

    async fn session_sender(&self, session_name: &str) -> broadcast::Sender<WorkspaceEvent> {
        match self.ensure_session_resources(session_name).await {
            Ok(resources) => resources.tx,
            Err(error) => {
                let tx = {
                    let mut sessions = self.inner.sessions.lock().await;
                    sessions
                        .entry(session_name.to_string())
                        .or_insert_with(SessionRuntime::new)
                        .tx
                        .clone()
                };
                broadcast_error(&tx, error);
                tx
            }
        }
    }

    async fn running_snapshot(&self, session_name: &str) -> Option<RunningTurnSnapshot> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(session_name)
            .and_then(|runtime| runtime.running.as_ref())
            .map(|running| RunningTurnSnapshot {
                turn_id: running.turn_id.clone(),
                pending_approval: sessions
                    .get(session_name)
                    .and_then(|runtime| runtime.approvals.front())
                    .map(|approval| approval.request.id.clone()),
            })
    }

    async fn session_has_active_work(&self, session_name: &str) -> bool {
        let supervisor = {
            let sessions = self.inner.sessions.lock().await;
            let Some(runtime) = sessions.get(session_name) else {
                return false;
            };
            if runtime.running.is_some() || !runtime.approvals.is_empty() {
                return true;
            }
            runtime.supervisor.clone()
        };
        match supervisor {
            Some(supervisor) => supervisor.has_active_runs().await,
            None => false,
        }
    }

    async fn approval_snapshots(&self, session_name: &str) -> Vec<ApprovalRequest> {
        self.inner
            .sessions
            .lock()
            .await
            .get(session_name)
            .map(|runtime| {
                runtime
                    .approvals
                    .iter()
                    .map(|approval| approval.request.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct StartTurnRequest {
    request_id: String,
    prompt: String,
    prompt_resolved: bool,
    permissions: PermissionProfile,
    model_selection: Option<ModelSelection>,
    resolved_model: Option<ResolvedModel>,
    mcp_servers: Option<Vec<McpServerConfig>>,
    subagent_identities: Option<Vec<SubagentIdentity>>,
    subagent_role_overrides: Option<BTreeMap<SubagentRole, SubagentRoleOverride>>,
    subagent_role_models: Option<BTreeMap<SubagentRole, ResolvedModel>>,
}

impl WorkspaceApp {
    async fn start_turn(
        &self,
        session_name: String,
        request: StartTurnRequest,
        tx: broadcast::Sender<WorkspaceEvent>,
    ) {
        let StartTurnRequest {
            request_id,
            prompt,
            prompt_resolved,
            permissions,
            model_selection,
            resolved_model,
            mcp_servers,
            subagent_identities,
            subagent_role_overrides,
            subagent_role_models,
        } = request;
        if self.inner.shutting_down.load(Ordering::Acquire) {
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnRejected {
                    request_id,
                    reason: "workspace is shutting down".to_string(),
                },
            );
            return;
        }
        let prompt = if prompt_resolved {
            prompt
        } else {
            match self
                .inner
                .command_registry
                .resolve(ResolveCommandRequest { input: prompt })
            {
                Ok(resolved) => resolved.prompt,
                Err(error) => {
                    broadcast_message(
                        &tx,
                        WorkspaceEvent::TurnRejected {
                            request_id,
                            reason: error.to_string(),
                        },
                    );
                    return;
                }
            }
        };
        if prompt.trim().is_empty() {
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnRejected {
                    request_id,
                    reason: "prompt must not be empty".to_string(),
                },
            );
            return;
        }
        let store = match self.session_store(&session_name) {
            Ok(store) => store,
            Err(error) => {
                broadcast_message(
                    &tx,
                    WorkspaceEvent::TurnRejected {
                        request_id,
                        reason: error.to_string(),
                    },
                );
                return;
            }
        };
        if store.is_archived() {
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnRejected {
                    request_id,
                    reason: format!(
                        "session {session_name:?} is archived; restore it before starting a turn"
                    ),
                },
            );
            return;
        }
        let turn_id = format!("turn-{}", agent_runtime::timestamp_ms());
        let cancellation = CancellationToken::new();
        if self.running_snapshot(&session_name).await.is_some() {
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnRejected {
                    request_id,
                    reason: "session already has a running turn".to_string(),
                },
            );
            return;
        }
        let persist_model_selection = resolved_model.is_none();
        let resolved_model = match resolved_model {
            Some(model) => model,
            None => match self
                .inner
                .model_registry
                .resolve_for_turn(&session_name, model_selection)
                .await
            {
                Ok(model) => model,
                Err(error) => {
                    broadcast_message(
                        &tx,
                        WorkspaceEvent::TurnRejected {
                            request_id,
                            reason: error.to_string(),
                        },
                    );
                    return;
                }
            },
        };
        if persist_model_selection
            && let Err(error) = self
                .inner
                .model_registry
                .set_session_selection(&session_name, resolved_model.selection.clone())
                .await
        {
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnRejected {
                    request_id,
                    reason: error.to_string(),
                },
            );
            return;
        }
        let subagent_identities = match subagent_identities {
            Some(identities) if identities.len() >= MIN_SUBAGENT_PROFILES => identities,
            Some(_) => {
                broadcast_message(
                    &tx,
                    WorkspaceEvent::TurnRejected {
                        request_id,
                        reason: format!(
                            "at least {MIN_SUBAGENT_PROFILES} subagent identities are required"
                        ),
                    },
                );
                return;
            }
            None => self.inner.subagent_registry.identities().await,
        };
        let subagent_role_overrides = match subagent_role_overrides {
            Some(overrides) => overrides,
            None => self.inner.subagent_registry.role_overrides().await,
        };
        let supervisor = match self
            .prepare_subagent_supervisor_with_runtime(
                &session_name,
                &resolved_model,
                permissions,
                &subagent_identities,
                subagent_role_overrides,
                subagent_role_models,
            )
            .await
        {
            Ok(supervisor) => supervisor,
            Err(error) => {
                broadcast_message(
                    &tx,
                    WorkspaceEvent::TurnRejected {
                        request_id,
                        reason: error,
                    },
                );
                return;
            }
        };
        {
            let mut sessions = self.inner.sessions.lock().await;
            let runtime = sessions
                .entry(session_name.clone())
                .or_insert_with(SessionRuntime::new);
            if runtime.running.is_some() {
                broadcast_message(
                    &tx,
                    WorkspaceEvent::TurnRejected {
                        request_id,
                        reason: "session already has a running turn".to_string(),
                    },
                );
                return;
            }
            let app_for_task = self.clone();
            let session_for_task = session_name.clone();
            let turn_for_task = turn_id.clone();
            let cancellation_for_task = cancellation.clone();
            let tx_for_task = tx.clone();
            let worker = tokio::spawn(async move {
                app_for_task
                    .run_turn_task(TurnTaskContext {
                        session_name: session_for_task,
                        turn_id: turn_for_task,
                        prompt,
                        permissions,
                        resolved_model,
                        mcp_servers,
                        subagent_identities,
                        supervisor,
                        tx: tx_for_task,
                        cancellation: cancellation_for_task,
                    })
                    .await;
            });
            let handle = worker.abort_handle();
            let app_for_supervisor = self.clone();
            let session_for_supervisor = session_name.clone();
            let turn_for_supervisor = turn_id.clone();
            let tx_for_supervisor = tx.clone();
            tokio::spawn(async move {
                app_for_supervisor
                    .supervise_turn_worker(
                        session_for_supervisor,
                        turn_for_supervisor,
                        tx_for_supervisor,
                        worker,
                    )
                    .await;
            });
            runtime.running = Some(RunningTurn {
                turn_id: turn_id.clone(),
                cancellation,
                handle,
            });
        }
        if let Ok(snapshot) = self.session_snapshot(&session_name).await {
            broadcast_message(&tx, snapshot);
        }
    }
}

struct TurnTaskContext {
    session_name: String,
    turn_id: String,
    prompt: String,
    permissions: PermissionProfile,
    resolved_model: ResolvedModel,
    mcp_servers: Option<Vec<McpServerConfig>>,
    subagent_identities: Vec<SubagentIdentity>,
    supervisor: Arc<SubagentSupervisor>,
    tx: broadcast::Sender<WorkspaceEvent>,
    cancellation: CancellationToken,
}

impl WorkspaceApp {
    async fn run_turn_task(&self, context: TurnTaskContext) {
        let tx = context.tx.clone();
        if let Err(error) = self.run_turn_task_inner(context).await {
            broadcast_error(&tx, error);
        }
    }

    async fn supervise_turn_worker(
        &self,
        session_name: String,
        turn_id: String,
        tx: broadcast::Sender<WorkspaceEvent>,
        worker: tokio::task::JoinHandle<()>,
    ) {
        if worker.await.is_err_and(|error| error.is_panic()) {
            broadcast_error(&tx, format!("turn {turn_id} worker panicked"));
        }
        self.cancel_matching_approvals(&session_name, &tx, |request| {
            matches!(
                &request.origin,
                ApprovalOrigin::ParentTurn {
                    turn_id: Some(pending_turn),
                    ..
                } if pending_turn == &turn_id
            )
        })
        .await;
        self.clear_running_turn(&session_name, &turn_id).await;
        if let Ok(snapshot) = self.session_snapshot(&session_name).await {
            broadcast_message(&tx, snapshot);
        }
    }

    async fn run_turn_task_inner(
        &self,
        context: TurnTaskContext,
    ) -> Result<(), agent_runtime::RuntimeError> {
        let TurnTaskContext {
            session_name,
            turn_id,
            prompt,
            permissions,
            resolved_model,
            mcp_servers,
            subagent_identities,
            supervisor,
            tx,
            cancellation,
        } = context;
        let options = self.inner.options.clone();
        let mcp_cache = self.inner.mcp_cache.read().await.clone();
        let mcp_servers = match mcp_servers {
            Some(servers) => servers,
            None => self.inner.mcp_registry.effective_servers().await,
        };
        let store = SessionStore::for_workspace(&options.workspace_root, &session_name)?;
        let mut session = store.load()?;
        let turn_index = session.turns.len();
        let mut handler = AppTurnHandler {
            app: self.clone(),
            session_name: session_name.clone(),
            turn_id,
            tx: tx.clone(),
        };
        let outcome = agent_runtime::run_agent_turn_with_subagent_controller(
            RunAgentTurnContext {
                client: &resolved_model.client,
                model: &resolved_model.invocation,
                subagent_identities: &subagent_identities,
                system_prompt: &options.system_prompt,
                context_config: options.context_config,
                model_limits: resolved_model.limits,
                workspace_root: &options.workspace_root,
                permissions,
                mcp_servers: &mcp_servers,
                mcp_cache: mcp_cache.as_ref(),
                session_name: &session_name,
                turn_index,
            },
            &mut session,
            &prompt,
            &mut handler,
            cancellation,
            supervisor,
        )
        .await?;
        if outcome.session_changed {
            store.save(&session)?;
            broadcast_message(
                &tx,
                WorkspaceEvent::TurnSaved {
                    session: session_name,
                    turn_index,
                },
            );
        }
        if let Some(error) = outcome.error {
            broadcast_error(&tx, error);
        }
        Ok(())
    }
}
