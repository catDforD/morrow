use crate::scenario::{ScenarioTool, ToolBehavior, ToolResponse};
use agent_core::{
    ToolApproval, ToolExecution, ToolExecutionContext, ToolExecutionMode, ToolFuture, ToolResult,
    ToolRuntime,
};
use agent_protocol::{
    ApprovalRequest, ToolCall, ToolCallKind, ToolDefinition, ToolDefinitionKind, ToolFunctionCall,
};
use futures_util::future::FutureExt;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// A snapshot of one executed tool call, including whether it was executed
/// with an approval decision attached.
#[derive(Debug, Clone)]
pub struct RecordedToolCall {
    pub call: ToolCall,
    pub approved: bool,
}

#[derive(Debug)]
struct ScenarioToolRuntimeInner {
    definitions: Vec<ToolDefinition>,
    modes: HashMap<String, ToolExecutionMode>,
    behaviors: HashMap<String, Mutex<VecDeque<ToolResponse>>>,
    calls: Arc<Mutex<Vec<RecordedToolCall>>>,
}

/// Deterministic `ToolRuntime` built from scenario tool definitions. Tool
/// behavior is scripted, and every call is recorded for assertions.
pub struct ScenarioToolRuntime {
    inner: Arc<ScenarioToolRuntimeInner>,
}

impl ScenarioToolRuntime {
    pub fn new(tools: &[ScenarioTool]) -> Self {
        let definitions = tools
            .iter()
            .map(|tool| ToolDefinition {
                kind: ToolDefinitionKind::Function,
                function: agent_protocol::ToolFunctionDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                },
            })
            .collect();

        let modes = tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.mode))
            .collect();

        let behaviors = tools
            .iter()
            .map(|tool| {
                let responses = match &tool.behavior {
                    ToolBehavior::Always(response) => VecDeque::from([response.clone()]),
                    ToolBehavior::Sequence(responses) => responses.clone().into(),
                };
                (tool.name.clone(), Mutex::new(responses))
            })
            .collect();

        Self {
            inner: Arc::new(ScenarioToolRuntimeInner {
                definitions,
                modes,
                behaviors,
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
        }
    }

    pub fn record_calls(&self) -> Arc<Mutex<Vec<RecordedToolCall>>> {
        Arc::clone(&self.inner.calls)
    }
}

impl ToolRuntime for ScenarioToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions.clone()
    }

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        self.inner
            .modes
            .get(&call.function.name)
            .copied()
            .unwrap_or(ToolExecutionMode::Concurrent)
    }

    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        let inner = Arc::clone(&self.inner);
        async move {
            let approved = approval.is_some();
            inner
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(RecordedToolCall {
                    call: call.clone(),
                    approved,
                });

            let Some(responses) = inner.behaviors.get(&call.function.name) else {
                return ToolExecution::Completed(unknown_tool_result(&call));
            };
            let response = {
                let mut queue = responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                }
            };

            match response {
                Some(ToolResponse::Ok(content)) => ToolExecution::Completed(ok_result(content)),
                Some(ToolResponse::Fail(message)) => {
                    ToolExecution::Completed(ToolResult::error(message))
                }
                Some(ToolResponse::Approval {
                    approved,
                    denied: _denied,
                }) => match approval {
                    Some(_) => ToolExecution::Completed(ok_result(approved)),
                    None => ToolExecution::ApprovalRequired(ApprovalRequest::shell_command(
                        format!("approval-{}", call.id),
                        "scenario scripted shell command",
                        "/workspace",
                        30,
                        "scenario approval",
                    )),
                },
                None => ToolExecution::Completed(ToolResult::error(format!(
                    "scenario tool {:?} has no scripted responses",
                    call.function.name
                ))),
            }
        }
        .boxed()
    }
}

fn ok_result(content: String) -> ToolResult {
    ToolResult {
        ok: true,
        content,
        error: None,
        summary: None,
    }
}

fn unknown_tool_result(call: &ToolCall) -> ToolResult {
    ToolResult::error(format!(
        "unknown tool {:?} (known scenario tools: none match)",
        call.function.name
    ))
}

/// Reconstructs the structured call object recorded by the harness (useful
/// for argument assertions).
pub fn function_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        kind: ToolCallKind::Function,
        function: ToolFunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn sequence_behavior_advances_and_repeats_last_response() {
        let tool = ScenarioTool::new(
            "flaky",
            "flaky read",
            ToolExecutionMode::Concurrent,
            ToolBehavior::sequence(vec![
                ToolResponse::Fail("boom".to_string()),
                ToolResponse::Ok("recovered".to_string()),
            ]),
        );
        let runtime = ScenarioToolRuntime::new(&[tool]);
        let calls = runtime.record_calls();

        for expected in ["boom", "recovered", "recovered"] {
            let call = function_call("call-1", "flaky", "{}");
            let execution = runtime
                .execute(call.clone(), None, ToolExecutionContext::default())
                .await;
            match expected {
                "boom" => {
                    assert!(matches!(&execution, ToolExecution::Completed(result) if !result.ok))
                }
                _ => assert!(
                    matches!(&execution, ToolExecution::Completed(result) if result.ok && result.content == expected)
                ),
            }
        }

        assert_eq!(calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn approval_behavior_requests_then_honors_approval() {
        let tool = ScenarioTool::new(
            "write",
            "write file",
            ToolExecutionMode::Serial,
            ToolBehavior::Always(ToolResponse::Approval {
                approved: "written".to_string(),
                denied: "denied".to_string(),
            }),
        );
        let runtime = ScenarioToolRuntime::new(&[tool]);
        let call = function_call("call-1", "write", "{}");

        let first = runtime
            .execute(call.clone(), None, ToolExecutionContext::default())
            .await;
        let ToolExecution::ApprovalRequired(request) = first else {
            panic!("expected approval request, got {first:?}");
        };
        assert_eq!(request.id, "approval-call-1");

        let approved = ToolApproval {
            decision: agent_protocol::ApprovalDecision::approve(request.id.clone()),
            request,
        };
        let second = runtime
            .execute(call, Some(approved), ToolExecutionContext::default())
            .await;
        assert!(
            matches!(&second, ToolExecution::Completed(result) if result.ok && result.content == "written")
        );
    }

    #[test]
    fn definitions_match_scenario_tools() {
        let runtime = ScenarioToolRuntime::new(&[ScenarioTool::new(
            "read",
            "read file",
            ToolExecutionMode::Concurrent,
            ToolBehavior::ok("content"),
        )
        .with_parameters(json!({"type": "object"}))]);

        let definitions = runtime.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].function.name, "read");
        assert_eq!(
            definitions[0].function.parameters,
            json!({"type": "object"})
        );
    }
}
