use crate::process::run_hook_command;
use crate::protocol::{
    HookCommandResponse, HookCommandResult, command_context, compaction_cause, decision_allowed,
    gate_decision, permission_decision, tool_result_json,
};
use crate::types::{
    HOOK_CONFIG_SCHEMA_VERSION, HookDefinition, HookEvent, MAX_OPERATION_CONTEXT_BYTES,
};
use agent_core::{
    AfterToolInput, AgentMiddleware, BeforeToolInput, ContextBlock, GateOutput, MiddlewareError,
    MiddlewareExecutionContext, MiddlewareFuture, ObservationOutput, PermissionOutput,
    PermissionRequestInput,
};
use agent_protocol::MiddlewareSource;
use agent_runtime::{
    BeforePromptInput, MiddlewareRegistry, PostCompactInput, PreCompactInput, RuntimeMiddleware,
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn register_hook(
    registry: &mut MiddlewareRegistry,
    definition: HookDefinition,
    source: MiddlewareSource,
    context_budget: Arc<AtomicUsize>,
) {
    let event = definition.event;
    let failure_mode = definition.failure_mode.into();
    let hook = Arc::new(CommandHook {
        definition,
        source,
        context_budget,
    });
    if event.is_tool_event() {
        registry.register_agent_with_failure_mode(hook, failure_mode);
    } else {
        registry.register_runtime_with_failure_mode(hook, failure_mode);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CommandHook {
    pub(crate) definition: HookDefinition,
    pub(crate) source: MiddlewareSource,
    pub(crate) context_budget: Arc<AtomicUsize>,
}

impl CommandHook {
    pub(crate) fn matches(
        &self,
        context: &MiddlewareExecutionContext,
        tool_name: Option<&str>,
    ) -> bool {
        let scope_matches = self
            .definition
            .agent_scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(&context.agent_scope));
        let tool_matches = self.definition.tool_names.as_ref().is_none_or(|names| {
            tool_name.is_some_and(|tool_name| names.iter().any(|name| name == tool_name))
        });
        scope_matches && tool_matches
    }

    pub(crate) async fn invoke(
        &self,
        context: MiddlewareExecutionContext,
        payload: Value,
    ) -> Result<HookCommandResult, MiddlewareError> {
        let invocation_id = context
            .invocation_id
            .clone()
            .ok_or_else(|| MiddlewareError::new("middleware invocation id is missing"))?;
        let request = json!({
            "schema_version": HOOK_CONFIG_SCHEMA_VERSION,
            "invocation_id": invocation_id,
            "event": self.definition.event,
            "context": command_context(&context),
            "payload": payload,
        });
        let input = serde_json::to_vec(&request).map_err(|error| {
            MiddlewareError::new(format!("failed to serialize hook input: {error}"))
        })?;
        let output = run_hook_command(
            &self.definition.command,
            &context.workspace_root,
            self.definition.timeout_secs,
            &context,
            input,
        )
        .await?;
        let response = serde_json::from_slice::<HookCommandResponse>(&output).map_err(|error| {
            MiddlewareError::new(format!("hook stdout is not one valid JSON result: {error}"))
        })?;
        let result = self.validate_response(response)?;
        self.reserve_context(&result.additional_context)?;
        Ok(result)
    }

    fn validate_response(
        &self,
        response: HookCommandResponse,
    ) -> Result<HookCommandResult, MiddlewareError> {
        if !decision_allowed(self.definition.event, response.decision) {
            return Err(MiddlewareError::new(format!(
                "decision {:?} is invalid for {}",
                response.decision,
                self.definition.event.as_str()
            )));
        }
        let additional_context = response
            .additional_context
            .into_iter()
            .filter_map(|block| {
                let content = block.into_content();
                let content = content.trim().to_string();
                (!content.is_empty()).then_some(ContextBlock::new(content))
            })
            .collect();
        Ok(HookCommandResult {
            decision: response.decision,
            reason: response.reason,
            additional_context,
        })
    }

    pub(crate) fn reserve_context(&self, blocks: &[ContextBlock]) -> Result<(), MiddlewareError> {
        let bytes = blocks
            .iter()
            .map(|block| block.content.len())
            .sum::<usize>();
        self.context_budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_OPERATION_CONTEXT_BYTES)
            })
            .map(|_| ())
            .map_err(|_| {
                MiddlewareError::new(format!(
                    "operation middleware context exceeds {MAX_OPERATION_CONTEXT_BYTES} bytes"
                ))
            })
    }
}

impl AgentMiddleware for CommandHook {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn source(&self) -> MiddlewareSource {
        self.source
    }

    fn before_tool(&self, input: BeforeToolInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::BeforeTool
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(input.context, json!({ "tool_call": input.tool_call }))
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn permission_request(
        &self,
        input: PermissionRequestInput,
    ) -> Option<MiddlewareFuture<PermissionOutput>> {
        if self.definition.event != HookEvent::PermissionRequest
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "tool_call": input.tool_call,
                            "approval_request": input.request,
                        }),
                    )
                    .await?;
                Ok(PermissionOutput {
                    decision: permission_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn after_tool(&self, input: AfterToolInput) -> Option<MiddlewareFuture<ObservationOutput>> {
        if self.definition.event != HookEvent::AfterTool
            || !self.matches(&input.context, Some(&input.tool_call.function.name))
        {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "tool_call": input.tool_call,
                            "tool_result": tool_result_json(input.result),
                        }),
                    )
                    .await?;
                Ok(ObservationOutput {
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }
}

impl RuntimeMiddleware for CommandHook {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn source(&self) -> MiddlewareSource {
        self.source
    }

    fn before_prompt(&self, input: BeforePromptInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::BeforePrompt || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(input.context, json!({ "prompt": input.prompt }))
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn pre_compact(&self, input: PreCompactInput) -> Option<MiddlewareFuture<GateOutput>> {
        if self.definition.event != HookEvent::PreCompact || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "cause": compaction_cause(input.cause),
                            "estimated_tokens": input.estimated_tokens,
                            "token_budget": input.token_budget,
                            "current_summary": input.current_summary,
                            "summarized_turns": input.summarized_turns,
                        }),
                    )
                    .await?;
                Ok(GateOutput {
                    decision: gate_decision(result.decision, result.reason),
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }

    fn post_compact(&self, input: PostCompactInput) -> Option<MiddlewareFuture<ObservationOutput>> {
        if self.definition.event != HookEvent::PostCompact || !self.matches(&input.context, None) {
            return None;
        }
        let this = self.clone();
        Some(
            async move {
                let result = this
                    .invoke(
                        input.context,
                        json!({
                            "cause": compaction_cause(input.cause),
                            "previous_summary": input.previous_summary,
                            "summary": input.summary,
                            "summarized_turns": input.summarized_turns,
                        }),
                    )
                    .await?;
                Ok(ObservationOutput {
                    additional_context: result.additional_context,
                })
            }
            .boxed(),
        )
    }
}
