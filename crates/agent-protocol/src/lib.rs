use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
pub const SESSION_DOCUMENT_SCHEMA_VERSION: u32 = 4;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action: ApprovalAction,
    pub reason: String,
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
        }
    }
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
}

impl ToolExecutionSummary {
    pub fn file_changes(files: Vec<FileChangeSummary>, diff: impl Into<String>) -> Self {
        Self {
            files,
            diff: Some(diff.into()),
            shell: None,
            error: None,
        }
    }

    pub fn shell(shell: ShellCommandSummary) -> Self {
        Self {
            files: Vec::new(),
            diff: None,
            shell: Some(shell),
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            files: Vec::new(),
            diff: None,
            shell: None,
            error: Some(error.into()),
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    Warning(String),
    ReasoningDelta(String),
    TextDelta(String),
    AgentMessage(String),
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

        assert_eq!(value["schema_version"], json!(4));
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
}
