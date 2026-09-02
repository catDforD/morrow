use super::*;

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
