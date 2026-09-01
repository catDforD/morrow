use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Concurrent,
    Serial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionKind {
    Standard,
    Subagent {
        task: String,
        identity: SubagentIdentity,
    },
}

#[derive(Debug, Clone)]
pub struct ToolApproval {
    pub decision: ApprovalDecision,
    pub request: ApprovalRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecution {
    Completed(ToolResult),
    ApprovalRequired(ApprovalRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub ok: bool,
    pub content: String,
    pub error: Option<String>,
    pub summary: Option<ToolExecutionSummary>,
}

impl ToolExecution {
    pub fn error(error: impl Into<String>) -> Self {
        Self::Completed(ToolResult::error(error))
    }
}

impl ToolResult {
    pub fn error(error: impl Into<String>) -> Self {
        let error = error.into();
        let content = serde_json::to_string(&serde_json::json!({
            "ok": false,
            "error": &error,
        }))
        .expect("tool error JSON must serialize");
        Self {
            ok: false,
            error: Some(error.clone()),
            content,
            summary: Some(ToolExecutionSummary::error(error)),
        }
    }
}

pub type ToolFuture = BoxFuture<'static, ToolExecution>;

#[derive(Debug, Clone, Default)]
pub struct ToolExecutionContext {
    pub cancellation: CancellationToken,
}

pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode;

    fn execution_kind(&self, _call: &ToolCall) -> ToolExecutionKind {
        ToolExecutionKind::Standard
    }

    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture;
}

#[derive(Debug)]
pub(crate) struct EmptyToolRuntime;

impl ToolRuntime for EmptyToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execute(
        &self,
        call: ToolCall,
        _approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        let name = call.function.name;
        async move { ToolExecution::error(format!("unknown tool {name:?}")) }.boxed()
    }
}

pub(crate) static EMPTY_TOOL_RUNTIME: EmptyToolRuntime = EmptyToolRuntime;

pub(crate) struct ToolCallOutcome {
    pub(crate) index: usize,
    pub(crate) tool_call: ToolCall,
    pub(crate) phase: ToolCallPhase,
    pub(crate) serial: bool,
}

pub(crate) enum ToolCallPhase {
    Before(GateRun),
    Execution {
        execution: ToolExecution,
        approval_attempted: bool,
    },
    Permission(PermissionRun),
    After {
        result: ToolResult,
        middleware: ObservationRun,
    },
}
