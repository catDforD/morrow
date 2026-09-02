use super::*;

pub const REMOTE_PROTOCOL_VERSION: u32 = 5;
pub const REMOTE_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceLocation {
    Local {
        path: PathBuf,
    },
    Wsl {
        distro: String,
        user: String,
        path: String,
    },
}

impl WorkspaceLocation {
    pub fn display_path(&self) -> String {
        match self {
            Self::Local { path } => path.display().to_string(),
            Self::Wsl { path, .. } => path.clone(),
        }
    }

    pub fn target_label(&self) -> String {
        match self {
            Self::Local { .. } => "Local".to_string(),
            Self::Wsl { distro, .. } => format!("{distro} · WSL"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteModelSpec {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_secs: u64,
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
    pub reasoning_profile: ReasoningProfile,
    pub supports_tools: bool,
    pub invocation: ModelInvocation,
}

impl std::fmt::Debug for RemoteModelSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteModelSpec")
            .field("base_url", &"<configured>")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("timeout_secs", &self.timeout_secs)
            .field("context_window_tokens", &self.context_window_tokens)
            .field("reserved_output_tokens", &self.reserved_output_tokens)
            .field("reasoning_profile", &self.reasoning_profile)
            .field("supports_tools", &self.supports_tools)
            .field("invocation", &self.invocation)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteModelConnectionSpec {
    pub base_url: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

impl std::fmt::Debug for RemoteModelConnectionSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteModelConnectionSpec")
            .field("base_url", &"<configured>")
            .field("api_key", &"<redacted>")
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteFallbackModelSpec {
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
    pub reasoning_profile: ReasoningProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteMcpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteMcpServerSummary {
    pub name: String,
    pub transport: RemoteMcpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub http_header_keys: Vec<String>,
    pub enabled: bool,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteMcpServerSpec {
    pub name: String,
    pub transport: RemoteMcpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
}

impl std::fmt::Debug for RemoteMcpServerSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteMcpServerSpec")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &format_args!("<{} entries>", self.args.len()))
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("url", &self.url.as_ref().map(|_| "<configured>"))
            .field(
                "http_headers",
                &self.http_headers.keys().collect::<Vec<_>>(),
            )
            .field("enabled", &self.enabled)
            .field("startup_timeout_sec", &self.startup_timeout_sec)
            .field("tool_timeout_sec", &self.tool_timeout_sec)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteWorkspaceConfiguration {
    pub fallback_model: Option<RemoteFallbackModelSpec>,
    pub fallback_mcp_servers: Vec<RemoteMcpServerSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "source", content = "data", rename_all = "snake_case")]
pub enum RemoteTurnModel {
    WorkspaceFallback { selection: ModelSelection },
    Managed(RemoteModelSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteTurnSpec {
    pub session: String,
    pub request_id: String,
    pub prompt: String,
    pub permission_mode: Option<PermissionMode>,
    pub model: RemoteTurnModel,
    pub managed_mcp_servers: Vec<RemoteMcpServerSpec>,
    pub subagent_identities: Vec<SubagentIdentity>,
    pub subagent_roles: Vec<RemoteSubagentRoleSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteSubagentRoleSpec {
    pub role: SubagentRole,
    pub overrides: SubagentRoleOverride,
    pub model: RemoteTurnModel,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RemoteSubagentMessageSpec {
    pub session: String,
    pub message: serde_json::Value,
    pub permission_mode: Option<PermissionMode>,
    pub subagent_identities: Vec<SubagentIdentity>,
    pub subagent_roles: Vec<RemoteSubagentRoleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_model: Option<RemoteTurnModel>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RemoteEnvelope {
    pub protocol_version: u32,
    pub channel_id: u32,
    pub request_id: String,
    pub message: RemoteMessage,
}

impl RemoteEnvelope {
    pub fn new(channel_id: u32, request_id: impl Into<String>, message: RemoteMessage) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            channel_id,
            request_id: request_id.into(),
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RemoteMessage {
    Hello(RemoteHello),
    HelloAck(RemoteHelloAck),
    Request(RemoteRequest),
    Response(RemoteResponse),
    Event(RemoteEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteHello {
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub pid: u32,
    pub role: RemoteRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteHelloAck {
    pub desktop_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRole {
    Host,
    WorkspaceAgent,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RemoteRequest {
    Ping,
    Activity,
    Environment,
    WorkspaceConfiguration,
    ListDirectory {
        path: Option<String>,
        show_hidden: bool,
    },
    OpenWorkspace {
        path: String,
    },
    CloseWorkspace,
    Http {
        method: String,
        path: String,
        body: Option<serde_json::Value>,
    },
    SubscribeSession {
        session: String,
        subscription_id: String,
    },
    UnsubscribeSession {
        subscription_id: String,
    },
    SessionMessage {
        session: String,
        message: serde_json::Value,
    },
    StartTurn {
        turn: Box<RemoteTurnSpec>,
    },
    SubagentMessage {
        command: Box<RemoteSubagentMessageSpec>,
    },
    InspectMcp {
        server: Box<RemoteMcpServerSpec>,
    },
    DiscoverModels {
        model: RemoteModelConnectionSpec,
    },
    Shutdown {
        cancel_running: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RemoteResponse {
    Pong,
    Ack,
    Activity(RemoteActivity),
    Environment(RemoteEnvironment),
    WorkspaceConfiguration(RemoteWorkspaceConfiguration),
    Directory(RemoteDirectoryListing),
    WorkspaceOpened(RemoteWorkspaceInfo),
    Http(RemoteHttpResponse),
    SessionSubscribed {
        subscription_id: String,
        snapshot: Box<SessionStreamFrame>,
    },
    SessionCommand {
        message: Box<SessionStreamFrame>,
    },
    Error(RemoteError),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RemoteEvent {
    SessionMessage {
        subscription_id: String,
        message: Box<SessionStreamFrame>,
    },
    WorkspaceLog {
        level: String,
        message: String,
    },
    WorkerExited {
        channel_id: u32,
        code: Option<i32>,
    },
    WorkspaceReconnected {
        channel_id: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteEnvironment {
    pub user: String,
    pub home: String,
    pub platform: String,
    pub arch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteActivity {
    pub running_turns: usize,
    pub pending_approvals: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteDirectoryListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<RemoteDirectoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub path: String,
    pub directory: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteWorkspaceInfo {
    pub channel_id: u32,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RemoteHttpResponse {
    pub status: u16,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}
