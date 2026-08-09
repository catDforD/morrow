use crate::types::HookEvent;
use agent_core::{
    ContextBlock, GateDecision, MiddlewareExecutionContext, PermissionDecision, ToolResult,
};
use agent_runtime::CompactionCause;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookDecision {
    Continue,
    Approve,
    Deny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookCommandResponse {
    pub decision: HookDecision,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub additional_context: Vec<HookContextValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum HookContextValue {
    Text(String),
    Block(HookContextObject),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookContextObject {
    content: String,
}

impl HookContextValue {
    pub fn into_content(self) -> String {
        match self {
            Self::Text(content) => content,
            Self::Block(block) => block.content,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HookCommandResult {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub additional_context: Vec<ContextBlock>,
}

pub(crate) fn gate_decision(decision: HookDecision, reason: Option<String>) -> GateDecision {
    match decision {
        HookDecision::Deny => GateDecision::Deny {
            reason: reason.unwrap_or_else(|| "denied by command hook".to_string()),
        },
        HookDecision::Continue | HookDecision::Approve => GateDecision::Continue,
    }
}

pub(crate) fn permission_decision(
    decision: HookDecision,
    reason: Option<String>,
) -> PermissionDecision {
    match decision {
        HookDecision::Continue => PermissionDecision::Continue,
        HookDecision::Approve => PermissionDecision::Approve { reason },
        HookDecision::Deny => PermissionDecision::Deny {
            reason: reason.unwrap_or_else(|| "denied by command hook".to_string()),
        },
    }
}

pub(crate) fn command_context(context: &MiddlewareExecutionContext) -> Value {
    json!({
        "session": context.session,
        "workspace_root": context.workspace_root,
        "turn_index": context.turn_index,
        "operation_id": context.operation_id,
        "turn_id": context.turn_id,
        "model": context.model,
        "permissions": context.permissions,
        "agent_scope": context.agent_scope,
    })
}

pub(crate) fn tool_result_json(result: ToolResult) -> Value {
    json!({
        "ok": result.ok,
        "content": result.content,
        "error": result.error,
        "summary": result.summary,
    })
}

pub(crate) fn compaction_cause(cause: CompactionCause) -> &'static str {
    match cause {
        CompactionCause::Automatic => "automatic",
        CompactionCause::Manual => "manual",
    }
}

pub(crate) fn decision_allowed(event: HookEvent, decision: HookDecision) -> bool {
    match decision {
        HookDecision::Continue => true,
        HookDecision::Approve => event == HookEvent::PermissionRequest,
        HookDecision::Deny => matches!(
            event,
            HookEvent::BeforePrompt
                | HookEvent::BeforeTool
                | HookEvent::PermissionRequest
                | HookEvent::PreCompact
        ),
    }
}
