use super::*;

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
