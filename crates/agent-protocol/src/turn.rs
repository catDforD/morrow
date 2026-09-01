use super::*;

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
