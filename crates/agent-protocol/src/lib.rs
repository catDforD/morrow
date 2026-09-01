use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const REMOTE_PROTOCOL_VERSION: u32 = 5;
pub const REMOTE_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SUBAGENT_PROMPT_SUFFIX_CHARS: usize = 4_000;
pub const MIN_SUBAGENT_TIMEOUT_SECS: u64 = 30;
pub const MAX_SUBAGENT_TIMEOUT_SECS: u64 = 1_800;
pub const MIN_SUBAGENT_TOOL_ROUNDS: usize = 1;
pub const MAX_SUBAGENT_TOOL_ROUNDS: usize = 99;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct SubagentIdentity {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRole {
    Explore,
    Plan,
    Worker,
    Reviewer,
}

impl SubagentRole {
    pub const ALL: [Self; 4] = [Self::Explore, Self::Plan, Self::Worker, Self::Reviewer];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRoleOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_suffix: String,
    pub timeout_secs: u64,
    pub max_tool_rounds: usize,
}

impl Default for SubagentRoleOverride {
    fn default() -> Self {
        Self {
            model_selection: None,
            prompt_suffix: String::new(),
            timeout_secs: 300,
            max_tool_rounds: 99,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentInstanceStatus {
    Idle,
    Queued,
    Running,
    WaitingApproval,
    Interrupted,
    Failed,
    Cancelled,
}

impl SubagentInstanceStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::WaitingApproval)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRunStatus {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl SubagentRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRunSummary {
    pub instance_id: String,
    pub run_id: String,
    pub role: SubagentRole,
    pub status: SubagentRunStatus,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub model_calls: usize,
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<FileChangeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell_commands: Vec<ShellCommandSummary>,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRunRecord {
    pub id: String,
    pub task: String,
    pub status: SubagentRunStatus,
    pub turn_index: usize,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SubagentRunSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentInstanceSnapshot {
    pub id: String,
    pub role: SubagentRole,
    pub identity: SubagentIdentity,
    pub status: SubagentInstanceStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_summary: Option<SubagentRunSummary>,
    #[serde(default)]
    pub event_log_truncated: bool,
}

pub fn default_subagent_identities() -> Vec<SubagentIdentity> {
    const NAMES: &[&str] = &[
        "后藤一里",
        "山田凉",
        "喜多郁代",
        "伊地知虹夏",
        "中野梓",
        "平泽唯",
        "琴吹䌷",
        "秋山澪",
        "田井中律",
        "井芹仁菜",
        "河原木桃香",
        "安和昴",
        "海老冢智",
        "露帕",
        "高松灯",
        "千早爱音",
        "要乐奈",
        "长崎爽世",
        "椎名立希",
        "丰川祥子",
        "若叶睦",
        "三角初华",
    ];
    NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| SubagentIdentity {
            id: format!("builtin-{:02}", index + 1),
            name: (*name).to_string(),
        })
        .collect()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls_with_content(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn with_reasoning_content(mut self, reasoning_content: impl Into<String>) -> Self {
        let reasoning_content = reasoning_content.into();
        self.reasoning_content = (!reasoning_content.is_empty()).then_some(reasoning_content);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: ToolDefinitionKind,
    pub function: ToolFunctionDefinition,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: ToolDefinitionKind::Function,
            function: ToolFunctionDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDefinitionKind {
    Function,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub function: ToolFunctionCall,
}

impl ToolCall {
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: ToolCallKind::Function,
            function: ToolFunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallKind {
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_system_prompt(system_prompt: impl Into<String>) -> Self {
        let mut conversation = Self::new();
        conversation.push(Message::system(system_prompt));
        conversation
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Thread {
    pub messages: Vec<Message>,
}

impl Thread {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

pub const THREAD_DOCUMENT_SCHEMA_VERSION: u32 = 2;
pub const SESSION_DOCUMENT_SCHEMA_VERSION: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ThreadDocument {
    pub schema_version: u32,
    pub thread: Thread,
}

impl ThreadDocument {
    pub fn new(thread: Thread) -> Self {
        Self {
            schema_version: THREAD_DOCUMENT_SCHEMA_VERSION,
            thread,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub summarized_turns: usize,
}

impl SessionContext {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Session {
    pub active_thread: Thread,
    pub turns: Vec<TurnRecord>,
    pub context: SessionContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionApplyError;

impl std::fmt::Display for SessionApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cannot apply a running turn to a session")
    }
}

impl std::error::Error for SessionApplyError {}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_thread(active_thread: Thread) -> Self {
        Self {
            active_thread,
            turns: Vec::new(),
            context: SessionContext::new(),
        }
    }

    /// 一次性记录 turn；只有成功完成的消息才进入下一轮模型上下文。
    pub fn apply_turn(&mut self, record: TurnRecord) {
        self.try_apply_turn(record)
            .expect("only terminal turn records may be applied");
    }

    pub fn try_apply_turn(&mut self, record: TurnRecord) -> Result<(), SessionApplyError> {
        if record.turn.status == TurnStatus::Running {
            return Err(SessionApplyError);
        }
        if record.turn.status == TurnStatus::Completed {
            self.active_thread
                .messages
                .extend(record.messages.iter().cloned());
        }
        self.turns.push(record);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionDocument {
    pub schema_version: u32,
    pub session: Session,
}

impl SessionDocument {
    pub fn new(session: Session) -> Self {
        Self {
            schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
            session,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl PermissionMode {
    /// Severity rank: read_only < workspace_write < danger_full_access.
    pub fn severity(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::WorkspaceWrite => 1,
            Self::DangerFullAccess => 2,
        }
    }

    /// The more restrictive of `self` and `ceiling`.
    pub fn clamp(self, ceiling: Self) -> Self {
        if self.severity() <= ceiling.severity() {
            self
        } else {
            ceiling
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    #[default]
    Off,
    High,
    Max,
}

impl ReasoningLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningProfile {
    #[default]
    None,
    Deepseek,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning: ReasoningLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelInvocation {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub reasoning: ReasoningLevel,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellPolicy {
    Deny,
    Prompt,
    Allow,
}

impl ShellPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Prompt => "prompt",
            Self::Allow => "allow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionProfile {
    pub mode: PermissionMode,
    pub shell: ShellPolicy,
}

impl PermissionProfile {
    pub fn for_mode(mode: PermissionMode) -> Self {
        Self {
            mode,
            shell: match mode {
                PermissionMode::ReadOnly | PermissionMode::WorkspaceWrite => ShellPolicy::Prompt,
                PermissionMode::DangerFullAccess => ShellPolicy::Allow,
            },
        }
    }
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::for_mode(PermissionMode::ReadOnly)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalAction {
    ShellCommand {
        command: String,
        cwd: PathBuf,
        timeout_secs: u64,
    },
    FileChanges {
        files: Vec<FileChangeSummary>,
        diff: String,
    },
    McpTool {
        server: String,
        tool: String,
        arguments: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalOrigin {
    #[default]
    Unknown,
    ParentTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    SubagentRun {
        instance_id: String,
        run_id: String,
        role: SubagentRole,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
}

impl ApprovalOrigin {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action: ApprovalAction,
    pub reason: String,
    #[serde(default, skip_serializing_if = "ApprovalOrigin::is_unknown")]
    pub origin: ApprovalOrigin,
}

impl ApprovalRequest {
    pub fn shell_command(
        id: impl Into<String>,
        command: impl Into<String>,
        cwd: impl Into<PathBuf>,
        timeout_secs: u64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            action: ApprovalAction::ShellCommand {
                command: command.into(),
                cwd: cwd.into(),
                timeout_secs,
            },
            reason: reason.into(),
            origin: ApprovalOrigin::Unknown,
        }
    }

    pub fn file_changes(
        id: impl Into<String>,
        files: Vec<FileChangeSummary>,
        diff: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            action: ApprovalAction::FileChanges {
                files,
                diff: diff.into(),
            },
            reason: reason.into(),
            origin: ApprovalOrigin::Unknown,
        }
    }

    pub fn mcp_tool(
        id: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
        arguments: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            action: ApprovalAction::McpTool {
                server: server.into(),
                tool: tool.into(),
                arguments: truncate_mcp_arguments(&arguments.into()),
            },
            reason: reason.into(),
            origin: ApprovalOrigin::Unknown,
        }
    }

    pub fn with_origin(mut self, origin: ApprovalOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// MCP 工具审批请求展示的序列化参数上限，避免超大参数淹没审批界面。
pub const MCP_ARGUMENTS_MAX_BYTES: usize = 2048;

fn truncate_mcp_arguments(arguments: &str) -> String {
    if arguments.len() <= MCP_ARGUMENTS_MAX_BYTES {
        return arguments.to_string();
    }
    let mut end = MCP_ARGUMENTS_MAX_BYTES;
    while !arguments.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &arguments[..end])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub approved: bool,
}

impl ApprovalDecision {
    pub fn approve(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            approved: true,
        }
    }

    pub fn deny(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            approved: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeOperation {
    Add,
    Update,
    Delete,
}

impl FileChangeOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FileChangeSummary {
    pub path: String,
    pub operation: FileChangeOperation,
    pub replacements: usize,
    pub created: bool,
    pub overwritten: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellCommandSummary {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentExecutionSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub truncated: bool,
}

impl SubagentExecutionSummary {
    pub fn success(
        task: impl Into<String>,
        result: impl Into<String>,
        model_calls: usize,
        tool_calls: usize,
        truncated: bool,
    ) -> Self {
        Self {
            agent_id: None,
            agent_name: None,
            task: task.into(),
            result: Some(result.into()),
            error: None,
            model_calls,
            tool_calls,
            truncated,
        }
    }

    pub fn failure(
        task: impl Into<String>,
        error: impl Into<String>,
        model_calls: usize,
        tool_calls: usize,
    ) -> Self {
        Self {
            agent_id: None,
            agent_name: None,
            task: task.into(),
            result: None,
            error: Some(error.into()),
            model_calls,
            tool_calls,
            truncated: false,
        }
    }

    pub fn with_agent_name(mut self, agent_name: impl Into<String>) -> Self {
        self.agent_name = Some(agent_name.into());
        self
    }

    pub fn with_agent_identity(mut self, identity: &SubagentIdentity) -> Self {
        self.agent_id = Some(identity.id.clone());
        self.agent_name = Some(identity.name.clone());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolExecutionSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileChangeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellCommandSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<Box<SubagentExecutionSummary>>,
}

impl ToolExecutionSummary {
    pub fn file_changes(files: Vec<FileChangeSummary>, diff: impl Into<String>) -> Self {
        Self {
            files,
            diff: Some(diff.into()),
            shell: None,
            error: None,
            subagent: None,
        }
    }

    pub fn shell(shell: ShellCommandSummary) -> Self {
        Self {
            files: Vec::new(),
            diff: None,
            shell: Some(shell),
            error: None,
            subagent: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            files: Vec::new(),
            diff: None,
            shell: None,
            error: Some(error.into()),
            subagent: None,
        }
    }

    pub fn subagent(subagent: SubagentExecutionSummary) -> Self {
        Self {
            files: Vec::new(),
            diff: None,
            shell: None,
            error: None,
            subagent: Some(Box::new(subagent)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepKind {
    ModelCall,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TurnStep {
    pub kind: TurnStepKind,
    pub status: TurnStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub error: Option<String>,
}

impl TurnStep {
    pub fn running(kind: TurnStepKind) -> Self {
        Self {
            kind,
            status: TurnStatus::Running,
            tool_name: None,
            tool_call_id: None,
            error: None,
        }
    }

    pub fn running_model_call() -> Self {
        Self::running(TurnStepKind::ModelCall)
    }

    pub fn running_tool_call(name: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: TurnStepKind::ToolCall,
            status: TurnStatus::Running,
            tool_name: Some(name.into()),
            tool_call_id: Some(id.into()),
            error: None,
        }
    }

    pub fn complete(&mut self) {
        self.status = TurnStatus::Completed;
        self.error = None;
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = TurnStatus::Failed;
        self.error = Some(error.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Turn {
    pub status: TurnStatus,
    pub user_message: Message,
    pub assistant_message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelInvocation>,
    pub steps: Vec<TurnStep>,
    pub error: Option<String>,
}

impl Turn {
    pub fn running(user_message: Message) -> Self {
        Self {
            status: TurnStatus::Running,
            user_message,
            assistant_message: None,
            model: None,
            steps: vec![TurnStep::running_model_call()],
            error: None,
        }
    }

    pub fn complete(&mut self, assistant_message: Message) {
        self.status = TurnStatus::Completed;
        self.assistant_message = Some(assistant_message);
        self.error = None;
        if let Some(step) = self.steps.last_mut() {
            step.complete();
        }
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.status = TurnStatus::Failed;
        self.error = Some(error.clone());
        // 并发工具可能同时处于 Running；turn 收束后不能留下“仍在运行”的持久化状态。
        for step in self
            .steps
            .iter_mut()
            .filter(|step| step.status == TurnStatus::Running)
        {
            step.fail(error.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TurnRecord {
    pub turn: Turn,
    pub messages: Vec<Message>,
}

impl TurnRecord {
    pub fn new(turn: Turn, messages: Vec<Message>) -> Self {
        Self { turn, messages }
    }

    pub fn failed_user_prompt(prompt: impl Into<String>, error: impl Into<String>) -> Self {
        let user_message = Message::user(prompt.into());
        let mut turn = Turn::running(user_message.clone());
        turn.fail(error);
        Self {
            turn,
            messages: vec![user_message],
        }
    }
}

pub const SESSION_STREAM_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionLogHeader {
    pub schema_version: u32,
    pub session_id: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionFactEnvelope {
    pub revision: u64,
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub fact: SessionFact,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionFact {
    TurnStarted {
        user_message: Message,
        model: ModelInvocation,
        permissions: PermissionProfile,
        /// 当次模型实际看到的完整 system prompt（含 AGENTS.md 与 subagent guidance）。
        /// v6 及更早的日志行没有此字段，反序列化为空串。
        #[serde(default)]
        system_prompt: String,
    },
    NoticeRecorded {
        message: String,
    },
    ModelCallStarted {
        model_call_id: String,
    },
    ModelMessageCommitted {
        model_call_id: String,
        message: Message,
    },
    ToolCallStarted {
        tool_call: ToolCall,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        decision: ApprovalDecision,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: Message,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<ToolExecutionSummary>,
    },
    TurnCompleted,
    TurnFailed {
        error: String,
    },
    TurnCancelled {
        reason: String,
    },
    TurnInterrupted {
        reason: String,
    },
    ContextCompacted {
        summary: String,
        covered_through_turn_id: String,
    },
    MiddlewareFinished {
        invocation: MiddlewareInvocationFinished,
    },
    /// before_prompt middleware 拒绝的 prompt。只作审计，不进入投影的模型上下文或
    /// Turn 状态机。
    PromptRejected {
        prompt: String,
        reasons: Vec<String>,
    },
    LegacyContextCheckpoint {
        source_schema: u32,
        messages: Vec<Message>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStepStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStepKind {
    ModelCall,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStepProjection {
    pub id: String,
    pub kind: SessionStepKind,
    pub status: SessionStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_summary: Option<ToolExecutionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_decision: Option<ApprovalDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TurnProjection {
    pub id: String,
    pub operation_id: String,
    pub index: usize,
    pub status: SessionTurnStatus,
    pub user_message: Message,
    pub model: ModelInvocation,
    pub permissions: PermissionProfile,
    pub messages: Vec<Message>,
    pub steps: Vec<SessionStepProjection>,
    #[serde(default)]
    pub notices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelContextProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_through_turn_id: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub legacy_checkpoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionProjection {
    pub session_id: String,
    pub revision: u64,
    pub turns: Vec<TurnProjection>,
    pub context: ModelContextProjection,
    #[serde(default)]
    pub middleware_audit: Vec<MiddlewareInvocationFinished>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionProjectionDocument {
    pub schema_version: u32,
    pub session: SessionProjection,
}

impl SessionProjectionDocument {
    pub fn new(session: SessionProjection) -> Self {
        Self {
            schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
            session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StreamingMessageProjection {
    pub model_call_id: String,
    pub content: String,
    pub reasoning: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OperationProjection {
    pub operation_id: String,
    pub turn_id: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<StreamingMessageProjection>,
    pub cancellable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StreamCursor {
    pub stream_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    pub session_name: String,
    pub session_id: String,
    pub revision: u64,
    pub cursor: StreamCursor,
    pub session: SessionProjection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<OperationProjection>,
    pub permissions: PermissionProfile,
    pub approvals: Vec<ApprovalRequest>,
    pub subagents: Vec<SubagentInstanceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionUpdate {
    TurnUpserted(Box<TurnProjection>),
    ContextReplaced(ModelContextProjection),
    OperationReplaced(Option<OperationProjection>),
    ModelStreamDelta {
        operation_id: String,
        model_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    ApprovalsReplaced(Vec<ApprovalRequest>),
    SubagentUpserted(Box<SubagentInstanceSnapshot>),
    SubagentRemoved {
        instance_id: String,
    },
    MiddlewareRecorded(MiddlewareInvocationFinished),
    Notice {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionUpdateEnvelope {
    pub schema_version: u32,
    pub stream_id: String,
    pub sequence: u64,
    pub session_revision: u64,
    pub timestamp_ms: u64,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionStreamFrame {
    Snapshot(Box<SessionSnapshot>),
    Event(Box<SessionUpdateEnvelope>),
    ResyncRequired {
        reason: String,
    },
    CommandResult {
        request_id: String,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    CommandData {
        request_id: String,
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEventOrigin {
    #[default]
    Session,
    ParentTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        turn_index: usize,
    },
    SubagentRun {
        instance_id: String,
        run_id: String,
        role: SubagentRole,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_name: Option<String>,
        turn_index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareStage {
    BeforePrompt,
    BeforeTool,
    PermissionRequest,
    AfterTool,
    AfterTurn,
    PreCompact,
    PostCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareSource {
    Internal,
    UserCommand,
    ProjectCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareAgentScope {
    Main,
    DelegatedSubagent,
    PersistentSubagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareOutcome {
    Continue,
    Approve,
    Deny,
    FailedOpen,
    FailedClosed,
    Cancelled,
    SkippedUntrusted,
}

/// 一次 middleware 调用注入到模型请求的上下文块。定义在 protocol 层，使
/// `MiddlewareInvocationFinished` 与 Session fact log 能直接持久化它。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MiddlewareContextBlock {
    pub middleware_id: String,
    pub source: MiddlewareSource,
    pub stage: MiddlewareStage,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MiddlewareInvocationStarted {
    pub invocation_id: String,
    pub middleware_id: String,
    pub source: MiddlewareSource,
    pub stage: MiddlewareStage,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MiddlewareInvocationFinished {
    pub invocation_id: String,
    pub middleware_id: String,
    pub source: MiddlewareSource,
    pub stage: MiddlewareStage,
    pub outcome: MiddlewareOutcome,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 该次调用实际注入模型请求的上下文块；无注入或 v6 及更早的日志行为空。
    #[serde(default)]
    pub injected_context: Vec<MiddlewareContextBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    ModelCallStarted,
    MiddlewareStarted(MiddlewareInvocationStarted),
    MiddlewareFinished(MiddlewareInvocationFinished),
    Warning(String),
    ReasoningDelta(String),
    TextDelta(String),
    ModelMessageCommitted {
        model_call_id: String,
        message: Message,
    },
    AgentMessage(String),
    SubagentStarted {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        task: String,
    },
    SubagentFinished {
        id: String,
        ok: bool,
        summary: SubagentExecutionSummary,
    },
    SubagentUpdated(Box<SubagentInstanceSnapshot>),
    ToolCallStarted {
        id: String,
        name: String,
    },
    ToolCallFinished {
        id: String,
        name: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<ToolExecutionSummary>,
    },
    ToolResultCommitted {
        tool_call_id: String,
        message: Message,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<ToolExecutionSummary>,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved(ApprovalDecision),
    TurnCompleted,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_mode_clamp_picks_the_more_restrictive_mode() {
        assert_eq!(
            PermissionMode::DangerFullAccess.clamp(PermissionMode::WorkspaceWrite),
            PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            PermissionMode::ReadOnly.clamp(PermissionMode::DangerFullAccess),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::WorkspaceWrite.clamp(PermissionMode::WorkspaceWrite),
            PermissionMode::WorkspaceWrite
        );
        assert!(
            PermissionMode::ReadOnly.severity() < PermissionMode::WorkspaceWrite.severity()
                && PermissionMode::WorkspaceWrite.severity()
                    < PermissionMode::DangerFullAccess.severity()
        );
    }

    #[test]
    fn remote_runtime_debug_output_redacts_managed_secrets() {
        let model = RemoteModelSpec {
            base_url: "https://models.example/v1".to_string(),
            model: "example-model".to_string(),
            api_key: "model-secret".to_string(),
            timeout_secs: 30,
            context_window_tokens: 32_000,
            reserved_output_tokens: 4_000,
            reasoning_profile: ReasoningProfile::None,
            supports_tools: true,
            invocation: ModelInvocation {
                provider_id: "example".to_string(),
                provider_name: "Example".to_string(),
                model_id: "example-model".to_string(),
                model_name: "Example model".to_string(),
                reasoning: ReasoningLevel::Off,
            },
        };
        let mcp = RemoteMcpServerSpec {
            name: "docs".to_string(),
            transport: RemoteMcpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::from([("TOKEN".to_string(), "mcp-env-secret".to_string())]),
            cwd: None,
            url: Some("https://mcp.example".to_string()),
            http_headers: BTreeMap::from([(
                "Authorization".to_string(),
                "mcp-header-secret".to_string(),
            )]),
            enabled: true,
            startup_timeout_sec: 10,
            tool_timeout_sec: 60,
        };
        let debug = format!("{model:?} {mcp:?}");

        assert!(!debug.contains("model-secret"));
        assert!(!debug.contains("mcp-env-secret"));
        assert!(!debug.contains("mcp-header-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn remote_turn_protocol_v5_carries_middleware_compatible_runtime_data() {
        assert_eq!(REMOTE_PROTOCOL_VERSION, 5);
        let turn = RemoteTurnSpec {
            session: "default".to_string(),
            request_id: "request-1".to_string(),
            prompt: "inspect".to_string(),
            permission_mode: None,
            model: RemoteTurnModel::WorkspaceFallback {
                selection: ModelSelection {
                    provider_id: "provider".to_string(),
                    model_id: "model".to_string(),
                    reasoning: ReasoningLevel::Off,
                },
            },
            managed_mcp_servers: Vec::new(),
            subagent_identities: vec![SubagentIdentity {
                id: "builtin-01".to_string(),
                name: "后藤一里".to_string(),
            }],
            subagent_roles: Vec::new(),
        };

        let value = serde_json::to_value(turn).expect("serialize remote turn");
        assert_eq!(
            value["subagent_identities"],
            json!([{"id": "builtin-01", "name": "后藤一里"}])
        );
        assert!(!value.to_string().contains("avatar"));
    }

    #[test]
    fn remote_subagent_follow_up_can_supply_the_snapshotted_model_again() {
        let selection = ModelSelection {
            provider_id: "provider".to_string(),
            model_id: "old-model".to_string(),
            reasoning: ReasoningLevel::High,
        };
        let command = RemoteSubagentMessageSpec {
            session: "default".to_string(),
            message: json!({
                "type": "send_subagent",
                "data": {
                    "request_id": "request-2",
                    "instance_id": "subagent-1",
                    "message": "continue",
                    "model_selection": selection
                }
            }),
            permission_mode: Some(PermissionMode::WorkspaceWrite),
            subagent_identities: default_subagent_identities(),
            subagent_roles: Vec::new(),
            resume_model: Some(RemoteTurnModel::WorkspaceFallback {
                selection: selection.clone(),
            }),
        };

        let value = serde_json::to_value(&command).expect("serialize remote command");
        assert_eq!(
            value["resume_model"]["data"]["selection"]["model_id"],
            "old-model"
        );
        let decoded: RemoteSubagentMessageSpec =
            serde_json::from_value(value).expect("deserialize remote command");
        assert_eq!(decoded, command);

        let legacy: RemoteSubagentMessageSpec = serde_json::from_value(json!({
            "session": "default",
            "message": {"type": "spawn_subagent", "data": {}},
            "permission_mode": null,
            "subagent_identities": [],
            "subagent_roles": []
        }))
        .expect("legacy v3 command without resume model");
        assert!(legacy.resume_model.is_none());
    }

    #[test]
    fn serializes_messages_in_openai_chat_shape() {
        let mut conversation = Conversation::with_system_prompt("You are helpful.");
        conversation.push(Message::user("Hello"));
        conversation.push(Message::assistant("Hi"));

        let value = serde_json::to_value(&conversation.messages).expect("serialize messages");

        assert_eq!(
            value,
            json!([
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ])
        );
    }

    #[test]
    fn thread_serializes_long_term_messages_without_system_prompt() {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));

        let value = serde_json::to_value(&thread).expect("serialize thread");

        assert_eq!(
            value,
            json!({
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi"}
                ]
            })
        );
    }

    #[test]
    fn thread_document_serializes_versioned_thread() {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));

        let document = ThreadDocument::new(thread.clone());
        let value = serde_json::to_value(&document).expect("serialize thread document");

        assert_eq!(
            value,
            json!({
                "schema_version": 2,
                "thread": {
                    "messages": [
                        {"role": "user", "content": "Hello"},
                        {"role": "assistant", "content": "Hi"}
                    ]
                }
            })
        );

        let decoded =
            serde_json::from_value::<ThreadDocument>(value).expect("deserialize thread document");

        assert_eq!(decoded.schema_version, THREAD_DOCUMENT_SCHEMA_VERSION);
        assert_eq!(decoded.thread, thread);
    }

    #[test]
    fn session_document_serializes_versioned_session() {
        let mut active_thread = Thread::new();
        active_thread.push(Message::system("Session summary:\nKnown facts"));
        active_thread.push(Message::user("Continue"));
        let mut turn = Turn::running(Message::user("Hello"));
        turn.complete(Message::assistant("Hi"));
        let session = Session {
            active_thread: active_thread.clone(),
            turns: vec![TurnRecord::new(
                turn.clone(),
                vec![Message::user("Hello"), Message::assistant("Hi")],
            )],
            context: SessionContext {
                summary: Some("Known facts".to_string()),
                summarized_turns: 1,
            },
        };

        let document = SessionDocument::new(session.clone());
        let value = serde_json::to_value(&document).expect("serialize session document");

        assert_eq!(value["schema_version"], json!(7));
        assert_eq!(
            value["session"]["context"],
            json!({"summary": "Known facts", "summarized_turns": 1})
        );
        assert_eq!(
            value["session"]["active_thread"],
            serde_json::to_value(active_thread).expect("active thread")
        );

        let decoded =
            serde_json::from_value::<SessionDocument>(value).expect("deserialize session document");
        assert_eq!(decoded.schema_version, SESSION_DOCUMENT_SCHEMA_VERSION);
        assert_eq!(decoded.session, session);
    }

    #[test]
    fn session_projection_serializes_required_empty_arrays() {
        let projection = SessionProjection {
            session_id: "session-1".to_string(),
            revision: 1,
            turns: vec![TurnProjection {
                id: "turn-1".to_string(),
                operation_id: "operation-1".to_string(),
                index: 0,
                status: SessionTurnStatus::Running,
                user_message: Message::user("Hello"),
                model: ModelInvocation {
                    provider_id: "test".to_string(),
                    provider_name: "Test".to_string(),
                    model_id: "test-model".to_string(),
                    model_name: "Test model".to_string(),
                    reasoning: ReasoningLevel::Off,
                },
                permissions: PermissionProfile::default(),
                messages: vec![Message::user("Hello")],
                steps: Vec::new(),
                notices: Vec::new(),
                error: None,
                started_at_ms: 1,
                completed_at_ms: None,
            }],
            context: ModelContextProjection::default(),
            middleware_audit: Vec::new(),
            diagnostics: Vec::new(),
        };

        let value = serde_json::to_value(projection).expect("serialize session projection");

        assert_eq!(value["diagnostics"], json!([]));
        assert_eq!(value["turns"][0]["notices"], json!([]));
    }

    #[test]
    fn applying_completed_turn_updates_active_thread_and_history_once() {
        let mut session = Session::from_thread(Thread {
            messages: vec![Message::user("Previous"), Message::assistant("Context")],
        });
        let user_message = Message::user("Hello");
        let assistant_message = Message::assistant("Hi");
        let mut turn = Turn::running(user_message.clone());
        turn.complete(assistant_message.clone());
        let record = TurnRecord::new(turn, vec![user_message.clone(), assistant_message.clone()]);

        session.apply_turn(record.clone());

        assert_eq!(
            session.active_thread.messages,
            vec![
                Message::user("Previous"),
                Message::assistant("Context"),
                user_message,
                assistant_message,
            ]
        );
        assert_eq!(session.turns, vec![record]);
    }

    #[test]
    fn applying_failed_turn_updates_history_without_changing_active_thread() {
        let initial_thread = Thread {
            messages: vec![Message::user("Previous"), Message::assistant("Context")],
        };
        let mut session = Session::from_thread(initial_thread.clone());
        let record = TurnRecord::failed_user_prompt("Broken", "model error");

        session.apply_turn(record.clone());

        assert_eq!(session.active_thread, initial_thread);
        assert_eq!(session.turns, vec![record]);
    }

    #[test]
    fn running_turn_cannot_be_applied_to_session() {
        let mut session = Session::new();
        let user_message = Message::user("Still running");
        let record = TurnRecord::new(Turn::running(user_message.clone()), vec![user_message]);

        let error = session
            .try_apply_turn(record)
            .expect_err("running turn must be rejected");

        assert_eq!(error, SessionApplyError);
        assert!(session.turns.is_empty());
        assert!(session.active_thread.messages.is_empty());
    }

    #[test]
    fn permission_profile_defaults_shell_policy_by_mode() {
        assert_eq!(
            PermissionProfile::default(),
            PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Prompt,
            }
        );
        assert_eq!(
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite).shell,
            ShellPolicy::Prompt
        );
        assert_eq!(
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess).shell,
            ShellPolicy::Allow
        );
    }

    #[test]
    fn serializes_approval_events() {
        let request = ApprovalRequest::shell_command(
            "approval-call_1",
            "cargo test",
            "/repo",
            30,
            "shell command requires approval",
        );
        let decision = ApprovalDecision::approve("approval-call_1");
        let events = vec![
            AgentEvent::ApprovalRequested(request),
            AgentEvent::ApprovalResolved(decision),
        ];

        let value = serde_json::to_value(&events).expect("serialize approval events");

        assert_eq!(
            value,
            json!([
                {
                    "type": "approval_requested",
                    "data": {
                        "id": "approval-call_1",
                        "action": {
                            "kind": "shell_command",
                            "command": "cargo test",
                            "cwd": "/repo",
                            "timeout_secs": 30
                        },
                        "reason": "shell command requires approval"
                    }
                },
                {
                    "type": "approval_resolved",
                    "data": {
                        "request_id": "approval-call_1",
                        "approved": true
                    }
                }
            ])
        );
    }

    #[test]
    fn mcp_tool_approval_roundtrips_and_truncates_arguments() {
        let request = ApprovalRequest::mcp_tool(
            "approval-call_1",
            "docs",
            "search",
            r#"{"query":"morrow"}"#,
            "MCP tool requires approval",
        );

        let value = serde_json::to_value(&request).expect("serialize mcp approval");
        assert_eq!(
            value,
            json!({
                "id": "approval-call_1",
                "action": {
                    "kind": "mcp_tool",
                    "server": "docs",
                    "tool": "search",
                    "arguments": "{\"query\":\"morrow\"}"
                },
                "reason": "MCP tool requires approval"
            })
        );
        let parsed: ApprovalRequest =
            serde_json::from_value(value).expect("deserialize mcp approval");
        assert_eq!(parsed, request);

        let long = "x".repeat(MCP_ARGUMENTS_MAX_BYTES + 100);
        let truncated = ApprovalRequest::mcp_tool("approval-call_2", "docs", "search", long, "r");
        let ApprovalAction::McpTool { arguments, .. } = &truncated.action else {
            panic!("expected mcp tool action");
        };
        assert!(arguments.ends_with("…(truncated)"));
        assert!(arguments.len() <= MCP_ARGUMENTS_MAX_BYTES + "…(truncated)".len());

        // 多字节字符跨越截断边界时回退到字符边界，不产生非法 UTF-8。
        let boundary = format!("{}界", "a".repeat(MCP_ARGUMENTS_MAX_BYTES - 1));
        let truncated =
            ApprovalRequest::mcp_tool("approval-call_3", "docs", "search", boundary, "r");
        let ApprovalAction::McpTool { arguments, .. } = &truncated.action else {
            panic!("expected mcp tool action");
        };
        assert!(arguments.starts_with(&"a".repeat(MCP_ARGUMENTS_MAX_BYTES - 1)));
    }

    #[test]
    fn serializes_warning_event() {
        let event = AgentEvent::Warning("mcp server docs: failed to start".to_string());

        let value = serde_json::to_value(&event).expect("serialize warning event");

        assert_eq!(
            value,
            json!({
                "type": "warning",
                "data": "mcp server docs: failed to start"
            })
        );
    }

    #[test]
    fn serializes_model_call_started_event() {
        assert_eq!(
            serde_json::to_value(AgentEvent::ModelCallStarted).expect("serialize model call event"),
            json!({"type": "model_call_started"})
        );
    }

    #[test]
    fn serializes_file_change_approval_and_tool_summary() {
        let file = FileChangeSummary {
            path: "src/lib.rs".to_string(),
            operation: FileChangeOperation::Update,
            replacements: 2,
            created: false,
            overwritten: true,
            deleted: false,
        };
        let request = ApprovalRequest::file_changes(
            "approval-call_1",
            vec![file.clone()],
            "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n",
            "file changes require approval",
        );
        let event = AgentEvent::ToolCallFinished {
            id: "call_1".to_string(),
            name: "apply_patch".to_string(),
            ok: true,
            summary: Some(ToolExecutionSummary::file_changes(
                vec![file],
                "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n",
            )),
        };

        let value = serde_json::to_value(json!({
            "request": request,
            "event": event,
        }))
        .expect("serialize file approval");

        assert_eq!(
            value,
            json!({
                "request": {
                    "id": "approval-call_1",
                    "action": {
                        "kind": "file_changes",
                        "files": [{
                            "path": "src/lib.rs",
                            "operation": "update",
                            "replacements": 2,
                            "created": false,
                            "overwritten": true,
                            "deleted": false
                        }],
                        "diff": "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n"
                    },
                    "reason": "file changes require approval"
                },
                "event": {
                    "type": "tool_call_finished",
                    "data": {
                        "id": "call_1",
                        "name": "apply_patch",
                        "ok": true,
                        "summary": {
                            "files": [{
                                "path": "src/lib.rs",
                                "operation": "update",
                                "replacements": 2,
                                "created": false,
                                "overwritten": true,
                                "deleted": false
                            }],
                            "diff": "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn omits_empty_tool_execution_summary() {
        let event = AgentEvent::ToolCallFinished {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            ok: true,
            summary: None,
        };

        let value = serde_json::to_value(&event).expect("serialize event");

        assert_eq!(
            value,
            json!({
                "type": "tool_call_finished",
                "data": {
                    "id": "call_1",
                    "name": "read_file",
                    "ok": true
                }
            })
        );
    }

    #[test]
    fn serializes_assistant_tool_call_and_tool_result_messages() {
        let tool_call = ToolCall::function("call_1", "read_file", r#"{"path":"Cargo.toml"}"#);
        let messages = vec![
            Message::assistant_tool_calls(vec![tool_call]),
            Message::tool_result("call_1", r#"{"ok":true}"#),
        ];

        let value = serde_json::to_value(&messages).expect("serialize messages");

        assert_eq!(
            value,
            json!([
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "content": "{\"ok\":true}",
                    "tool_call_id": "call_1"
                }
            ])
        );
    }

    #[test]
    fn serializes_assistant_tool_call_message_with_content() {
        let tool_call = ToolCall::function("call_1", "read_file", r#"{"path":"Cargo.toml"}"#);
        let message =
            Message::assistant_tool_calls_with_content("I will read it.", vec![tool_call]);

        let value = serde_json::to_value(&message).expect("serialize message");

        assert_eq!(
            value,
            json!({
                "role": "assistant",
                "content": "I will read it.",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"Cargo.toml\"}"
                    }
                }]
            })
        );
    }

    #[test]
    fn turn_serializes_running_model_call_shape() {
        let turn = Turn::running(Message::user("Hello"));

        let value = serde_json::to_value(&turn).expect("serialize turn");

        assert_eq!(
            value,
            json!({
                "status": "running",
                "user_message": {"role": "user", "content": "Hello"},
                "assistant_message": null,
                "steps": [{
                    "kind": "model_call",
                    "status": "running",
                    "error": null
                }],
                "error": null
            })
        );
    }

    #[test]
    fn turn_records_completion_and_failure() {
        let mut completed = Turn::running(Message::user("Hello"));
        completed.complete(Message::assistant("Hi"));

        assert_eq!(completed.status, TurnStatus::Completed);
        assert_eq!(completed.assistant_message, Some(Message::assistant("Hi")));
        assert_eq!(completed.steps[0].status, TurnStatus::Completed);
        assert_eq!(completed.error, None);

        let mut failed = Turn::running(Message::user("Hello"));
        failed.fail("model error");

        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.assistant_message, None);
        assert_eq!(failed.steps[0].status, TurnStatus::Failed);
        assert_eq!(failed.steps[0].error, Some("model error".to_string()));
        assert_eq!(failed.error, Some("model error".to_string()));
    }

    #[test]
    fn failed_turn_closes_every_running_step() {
        let mut turn = Turn::running(Message::user("Hello"));
        turn.steps
            .push(TurnStep::running_tool_call("read_file", "call-1"));
        turn.steps
            .push(TurnStep::running_tool_call("list_files", "call-2"));

        turn.fail("turn cancelled");

        assert!(
            turn.steps
                .iter()
                .all(|step| step.status != TurnStatus::Running)
        );
        assert!(
            turn.steps
                .iter()
                .all(|step| step.error.as_deref() == Some("turn cancelled"))
        );
    }

    #[test]
    fn turn_record_preserves_messages_for_completed_and_failed_turns() {
        let mut completed = Turn::running(Message::user("Hello"));
        completed.complete(Message::assistant("Hi"));
        let record = TurnRecord::new(
            completed.clone(),
            vec![Message::user("Hello"), Message::assistant("Hi")],
        );

        assert_eq!(record.turn, completed);
        assert_eq!(record.messages.len(), 2);

        let failed = TurnRecord::failed_user_prompt("Broken", "model error");

        assert_eq!(failed.turn.status, TurnStatus::Failed);
        assert_eq!(failed.messages, vec![Message::user("Broken")]);
        assert_eq!(failed.turn.error.as_deref(), Some("model error"));
    }

    #[test]
    fn serializes_subagent_events_and_summary() {
        let summary = SubagentExecutionSummary::success(
            "Inspect session storage",
            "Sessions are scoped by workspace hash.",
            2,
            3,
            false,
        )
        .with_agent_identity(&SubagentIdentity {
            id: "builtin-01".to_string(),
            name: "后藤一里".to_string(),
        });
        let events = vec![
            AgentEvent::SubagentStarted {
                id: "call-1".to_string(),
                agent_id: summary.agent_id.clone(),
                agent_name: summary.agent_name.clone(),
                task: summary.task.clone(),
            },
            AgentEvent::SubagentFinished {
                id: "call-1".to_string(),
                ok: true,
                summary: summary.clone(),
            },
        ];

        assert_eq!(
            serde_json::to_value(events).expect("serialize subagent events"),
            json!([
                {
                    "type": "subagent_started",
                    "data": {
                        "id": "call-1",
                        "agent_id": "builtin-01",
                        "agent_name": "后藤一里",
                        "task": "Inspect session storage"
                    }
                },
                {
                    "type": "subagent_finished",
                    "data": {
                        "id": "call-1",
                        "ok": true,
                        "summary": {
                            "agent_id": "builtin-01",
                            "agent_name": "后藤一里",
                            "task": "Inspect session storage",
                            "result": "Sessions are scoped by workspace hash.",
                            "model_calls": 2,
                            "tool_calls": 3,
                            "truncated": false
                        }
                    }
                }
            ])
        );
        assert_eq!(
            serde_json::to_value(ToolExecutionSummary::subagent(summary))
                .expect("serialize subagent summary"),
            json!({
                "subagent": {
                    "agent_id": "builtin-01",
                    "agent_name": "后藤一里",
                    "task": "Inspect session storage",
                    "result": "Sessions are scoped by workspace hash.",
                    "model_calls": 2,
                    "tool_calls": 3,
                    "truncated": false
                }
            })
        );

        let legacy_event: AgentEvent = serde_json::from_value(json!({
            "type": "subagent_started",
            "data": {
                "id": "legacy-call",
                "task": "Inspect legacy state"
            }
        }))
        .expect("deserialize legacy subagent event");
        assert!(matches!(
            legacy_event,
            AgentEvent::SubagentStarted {
                agent_id: None,
                agent_name: None,
                ..
            }
        ));
    }

    #[test]
    fn v6_fact_lines_without_model_visible_fields_still_parse() {
        // v6 的 TurnStarted 没有 system_prompt，MiddlewareFinished 没有 injected_context。
        let turn_started: SessionFactEnvelope = serde_json::from_value(json!({
            "revision": 1,
            "timestamp_ms": 1,
            "operation_id": "operation-1",
            "turn_id": "turn-1",
            "fact": {
                "type": "turn_started",
                "data": {
                    "user_message": {"role": "user", "content": "hello"},
                    "model": {
                        "provider_id": "test",
                        "provider_name": "Test",
                        "model_id": "model",
                        "model_name": "Model",
                        "reasoning": "off"
                    },
                    "permissions": {"mode": "read_only", "shell": "deny"}
                }
            }
        }))
        .expect("parse v6 turn_started");
        assert!(matches!(
            turn_started.fact,
            SessionFact::TurnStarted {
                ref system_prompt,
                ..
            } if system_prompt.is_empty()
        ));

        let middleware_finished: SessionFactEnvelope = serde_json::from_value(json!({
            "revision": 2,
            "timestamp_ms": 2,
            "fact": {
                "type": "middleware_finished",
                "data": {
                    "invocation": {
                        "invocation_id": "middleware-1",
                        "middleware_id": "policy",
                        "source": "internal",
                        "stage": "before_prompt",
                        "outcome": "continue",
                        "started_at_ms": 1,
                        "duration_ms": 2
                    }
                }
            }
        }))
        .expect("parse v6 middleware_finished");
        assert!(matches!(
            middleware_finished.fact,
            SessionFact::MiddlewareFinished {
                ref invocation,
            } if invocation.injected_context.is_empty()
        ));
    }

    #[test]
    fn model_visible_fact_fields_roundtrip() {
        let invocation = MiddlewareInvocationFinished {
            invocation_id: "middleware-1".to_string(),
            middleware_id: "policy".to_string(),
            source: MiddlewareSource::ProjectCommand,
            stage: MiddlewareStage::BeforePrompt,
            outcome: MiddlewareOutcome::Continue,
            started_at_ms: 1,
            duration_ms: 2,
            reason: None,
            injected_context: vec![MiddlewareContextBlock {
                middleware_id: "policy".to_string(),
                source: MiddlewareSource::ProjectCommand,
                stage: MiddlewareStage::BeforePrompt,
                content: "injected".to_string(),
            }],
        };
        let facts = vec![
            SessionFact::TurnStarted {
                user_message: Message::user("hello"),
                model: ModelInvocation {
                    provider_id: "test".to_string(),
                    provider_name: "Test".to_string(),
                    model_id: "model".to_string(),
                    model_name: "Model".to_string(),
                    reasoning: ReasoningLevel::Off,
                },
                permissions: PermissionProfile::default(),
                system_prompt: "base\n\nguidance".to_string(),
            },
            SessionFact::MiddlewareFinished {
                invocation: invocation.clone(),
            },
            SessionFact::PromptRejected {
                prompt: "secret prompt".to_string(),
                reasons: vec!["policy: secret detected".to_string()],
            },
        ];

        for fact in facts {
            let bytes = serde_json::to_vec(&fact).expect("serialize fact");
            let parsed: SessionFact = serde_json::from_slice(&bytes).expect("parse fact");
            assert_eq!(parsed, fact);
        }
    }
}
