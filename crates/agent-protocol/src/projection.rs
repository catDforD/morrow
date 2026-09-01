use super::*;

pub const SESSION_STREAM_SCHEMA_VERSION: u32 = 3;

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
