use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalRequest, ModelSelection,
    PermissionProfile, ReasoningLevel, ReasoningProfile, Session, SubagentIdentity,
    SubagentInstanceSnapshot, SubagentRole, SubagentRoleOverride,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::completion::{PathCompletion, complete_workspace_paths};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub archived: bool,
    pub running: bool,
    pub model: Option<ModelSelection>,
    pub permissions: PermissionProfile,
}

impl SessionInfo {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            title: id.clone(),
            id,
            archived: false,
            running: false,
            model: None,
            permissions: PermissionProfile::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub info: SessionInfo,
    pub session: Session,
    pub subagents: Vec<SubagentInstanceSnapshot>,
    pub approvals: Vec<ApprovalRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider_id: String,
    pub model_id: String,
    pub label: String,
    pub supports_reasoning: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub sessions: Vec<SessionInfo>,
    pub active_session: Option<SessionSnapshot>,
    /// An empty model list is valid. The TUI starts and opens model settings.
    pub models: Vec<ModelOption>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextEstimate {
    pub used_tokens: usize,
    pub input_budget_tokens: usize,
    pub auto_compact_at_tokens: usize,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedModelSpec {
    pub id: String,
    pub name: String,
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
    pub supports_tools: bool,
    pub reasoning_profile: ReasoningProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModelDraft {
    pub model_id: String,
    pub reasoning: ReasoningLevel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderView {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_format: String,
    pub api_key_configured: bool,
    pub enabled: bool,
    pub read_only: bool,
    pub timeout_secs: u64,
    pub models: Vec<ManagedModelSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderDraft {
    /// Empty for a provider that has not been persisted yet.
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Empty means preserve the existing key.
    pub api_key: SecretValue,
    pub enabled: bool,
    pub read_only: bool,
    pub timeout_secs: u64,
    pub models: Vec<ManagedModelSpec>,
    /// When present, saving the provider also makes this model the global default.
    pub default_model: Option<DefaultModelDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerSource {
    RuntimeConfig,
    MorrowManaged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerView {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub header_keys: Vec<String>,
    pub endpoint: String,
    pub enabled: bool,
    pub startup_timeout_secs: u64,
    pub tool_timeout_secs: u64,
    pub read_only: bool,
    pub source: McpServerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDraft {
    /// The persisted name used to resolve preserved secrets. `None` creates a new server.
    pub original_name: Option<String>,
    pub name: String,
    pub transport: McpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub env: BTreeMap<String, SecretValue>,
    pub headers: BTreeMap<String, SecretValue>,
    pub enabled: bool,
    pub startup_timeout_secs: u64,
    pub tool_timeout_secs: u64,
    pub read_only: bool,
    pub source: McpServerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCommandView {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCommandDraft {
    pub original_name: Option<String>,
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentIdentityView {
    pub identity: SubagentIdentity,
    pub avatar_configured: bool,
    /// Only newly selected local files have a path. Persisted avatars never expose their data.
    pub avatar_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentIdentityDraft {
    pub original_id: Option<String>,
    pub identity: SubagentIdentity,
    pub avatar_path: Option<PathBuf>,
    /// Remove the persisted avatar when no replacement path is provided.
    pub remove_avatar: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRoleView {
    pub role: SubagentRole,
    pub settings: SubagentRoleOverride,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub providers: Vec<ModelProviderView>,
    pub models: Vec<ModelOption>,
    pub default_model: Option<ModelSelection>,
    pub mcp_servers: Vec<McpServerView>,
    pub commands: Vec<ManagedCommandView>,
    pub subagent_identities: Vec<SubagentIdentityView>,
    pub subagent_roles: Vec<SubagentRoleView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentTranscript {
    pub instance: SubagentInstanceSnapshot,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsCommand {
    SaveModelProvider(ModelProviderDraft),
    DeleteModelProvider {
        provider_id: String,
    },
    DiscoverModels {
        provider_id: String,
    },
    SetDefaultModel(ModelSelection),
    UpdateModelApiKey {
        provider_id: String,
        api_key: SecretValue,
    },
    SaveMcpServer(McpServerDraft),
    ImportMcpServers {
        source: String,
    },
    TestMcpServer {
        name: String,
    },
    TestMcpServerDraft(McpServerDraft),
    SetMcpEnabled {
        name: String,
        enabled: bool,
    },
    DeleteMcpServer {
        name: String,
    },
    SaveManagedCommand(ManagedCommandDraft),
    DeleteManagedCommand {
        name: String,
    },
    SaveSubagentIdentity(SubagentIdentityDraft),
    DeleteSubagentIdentity {
        id: String,
    },
    SaveSubagentRole(SubagentRoleView),
    ResetSubagentRoles,
    ResetSubagentProfiles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendCommand {
    CreateSession,
    LoadSession {
        session_id: String,
    },
    ResetSession {
        session_id: String,
    },
    ArchiveSession {
        session_id: String,
    },
    RestoreSession {
        session_id: String,
    },
    SetSessionModel {
        session_id: String,
        selection: ModelSelection,
    },
    StartTurn {
        session_id: String,
        prompt: String,
        model: Option<ModelSelection>,
        permissions: PermissionProfile,
    },
    CancelTurn {
        session_id: String,
    },
    ResolveApproval {
        session_id: String,
        decision: ApprovalDecision,
    },
    CompactSession {
        session_id: String,
    },
    FollowUpSubagent {
        session_id: String,
        instance_id: String,
        prompt: String,
    },
    CancelSubagent {
        session_id: String,
        instance_id: String,
    },
    DeleteSubagent {
        session_id: String,
        instance_id: String,
    },
    LoadSubagentTranscript {
        session_id: String,
        instance_id: String,
    },
    Settings(SettingsCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    Ack,
    Session(SessionSnapshot),
    SessionCreated(SessionSnapshot),
    Settings(SettingsSnapshot),
    SubagentTranscript(SubagentTranscript),
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEvent {
    Snapshot(WorkspaceSnapshot),
    SessionsChanged(Vec<SessionInfo>),
    SessionLoaded(SessionSnapshot),
    SessionRunning {
        session_id: String,
        running: bool,
    },
    ApprovalQueue {
        session_id: String,
        approvals: Vec<ApprovalRequest>,
    },
    SubagentsChanged {
        session_id: String,
        subagents: Vec<SubagentInstanceSnapshot>,
    },
    Agent {
        session_id: String,
        origin: AgentEventOrigin,
        event: AgentEvent,
    },
    /// The reducer reloads the persisted session when this arrives.
    TurnSaved {
        session_id: String,
    },
    SettingsChanged,
    BroadcastLagged,
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    pub message: String,
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<String> for BackendError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for BackendError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Strongly typed application boundary consumed by the TUI.
///
/// Implementations should make `recv_event` cancellation-safe: the event loop recreates
/// its future after terminal input wins a `select!` race.
#[async_trait]
pub trait WorkspaceBackend: Send + Sync + 'static {
    async fn snapshot(
        &self,
        preferred_session: Option<&str>,
    ) -> Result<WorkspaceSnapshot, BackendError>;

    async fn recv_event(&self) -> Result<WorkspaceEvent, BackendError>;

    async fn execute(&self, command: BackendCommand) -> Result<CommandResult, BackendError>;

    async fn load_settings(&self) -> Result<SettingsSnapshot, BackendError>;

    async fn estimate_context(
        &self,
        session_id: &str,
        draft: &str,
        model: Option<ModelSelection>,
        permissions: PermissionProfile,
    ) -> Result<ContextEstimate, BackendError>;

    async fn complete_paths(
        &self,
        workspace: &Path,
        query: &str,
    ) -> Result<Vec<PathCompletion>, BackendError> {
        Ok(complete_workspace_paths(workspace, query, 100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_drafts_redact_all_secret_values_from_debug() {
        let provider = ModelProviderDraft {
            id: String::new(),
            name: "provider".to_string(),
            base_url: "https://example.test/v1".to_string(),
            api_key: SecretValue::new("provider-secret"),
            enabled: true,
            read_only: false,
            timeout_secs: 120,
            models: Vec::new(),
            default_model: None,
        };
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), SecretValue::new("mcp-env-secret"));
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            SecretValue::new("mcp-header-secret"),
        );
        let mcp = McpServerDraft {
            original_name: None,
            name: "server".to_string(),
            transport: McpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            url: Some("https://example.test/mcp".to_string()),
            env,
            headers,
            enabled: true,
            startup_timeout_secs: 10,
            tool_timeout_secs: 60,
            read_only: false,
            source: McpServerSource::MorrowManaged,
        };

        let debug = format!("{provider:?} {mcp:?}");
        assert!(!debug.contains("provider-secret"));
        assert!(!debug.contains("mcp-env-secret"));
        assert!(!debug.contains("mcp-header-secret"));
        assert_eq!(debug.matches("<redacted>").count(), 3);
    }
}
