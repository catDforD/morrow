use crate::types::HookEvent;
use agent_core::{
    AfterTurnOutput, ContextBlock, GateDecision, MiddlewareExecutionContext, PermissionDecision,
    ToolResult,
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
    Complete,
    Fail,
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
        // Complete/Fail 只对 after_turn 合法；decision_allowed 已在响应校验阶段拦截，
        // 这里防御性地按"无意见"处理。
        HookDecision::Continue
        | HookDecision::Approve
        | HookDecision::Complete
        | HookDecision::Fail => GateDecision::Continue,
    }
}

pub(crate) fn permission_decision(
    decision: HookDecision,
    reason: Option<String>,
) -> PermissionDecision {
    match decision {
        HookDecision::Approve => PermissionDecision::Approve { reason },
        HookDecision::Deny => PermissionDecision::Deny {
            reason: reason.unwrap_or_else(|| "denied by command hook".to_string()),
        },
        // 同上：Complete/Fail 不会到达这里，防御性按 Continue 处理。
        HookDecision::Continue | HookDecision::Complete | HookDecision::Fail => {
            PermissionDecision::Continue
        }
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
        HookDecision::Complete | HookDecision::Fail => event == HookEvent::AfterTurn,
    }
}

/// after_turn hook 的裁决映射：`complete` 接受完成，`continue` 打回并注入验证反馈，
/// `fail` 判负。`decision_allowed` 已在响应校验阶段拦截其他 decision，这里不会遇到。
pub(crate) fn after_turn_output(result: HookCommandResult) -> AfterTurnOutput {
    match result.decision {
        HookDecision::Continue => AfterTurnOutput::Continue {
            context: result.additional_context,
        },
        HookDecision::Fail => AfterTurnOutput::Fail {
            reason: result
                .reason
                .unwrap_or_else(|| "failed by command hook".to_string()),
        },
        HookDecision::Complete | HookDecision::Approve | HookDecision::Deny => {
            AfterTurnOutput::Complete
        }
    }
}
