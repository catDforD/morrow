use crate::middleware_runner::{
    MiddlewareCompletion, MiddlewareMetadata, attributed_reason, collect_context,
    run_middleware_chain,
};
use crate::{CancellationToken, ToolResult};
pub use agent_protocol::MiddlewareContextBlock;
use agent_protocol::{
    ApprovalRequest, MiddlewareAgentScope, MiddlewareOutcome, MiddlewareSource, MiddlewareStage,
    ModelInvocation, PermissionProfile, ToolCall,
};
use futures_util::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

static MIDDLEWARE_INVOCATION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    Open,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Continue,
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Continue,
    Approve { reason: Option<String> },
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    pub content: String,
}

impl ContextBlock {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MiddlewareExecutionContext {
    pub invocation_id: Option<String>,
    pub session: String,
    pub workspace_root: PathBuf,
    pub turn_index: usize,
    pub operation_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: ModelInvocation,
    pub permissions: PermissionProfile,
    pub agent_scope: MiddlewareAgentScope,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct BeforeToolInput {
    pub context: MiddlewareExecutionContext,
    pub tool_call: ToolCall,
}

#[derive(Debug, Clone)]
pub struct PermissionRequestInput {
    pub context: MiddlewareExecutionContext,
    pub tool_call: ToolCall,
    pub request: ApprovalRequest,
}

#[derive(Debug, Clone)]
pub struct AfterToolInput {
    pub context: MiddlewareExecutionContext,
    pub tool_call: ToolCall,
    pub result: ToolResult,
}

/// after_turn 切割面的输入：模型自称完成时的精简摘要，避免把整段历史塞给 middleware。
#[derive(Debug, Clone)]
pub struct AfterTurnInput {
    pub context: MiddlewareExecutionContext,
    /// 模型本轮的最终文本。
    pub final_text: String,
    /// 本 turn 内已执行的工具调用数。
    pub tool_call_count: usize,
    /// 本 turn 已提交的消息条数。
    pub turn_message_count: usize,
    /// 本 turn 调用过的工具名（按调用顺序）。
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutput {
    pub decision: GateDecision,
    pub additional_context: Vec<ContextBlock>,
}

impl Default for GateOutput {
    fn default() -> Self {
        Self {
            decision: GateDecision::Continue,
            additional_context: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOutput {
    pub decision: PermissionDecision,
    pub additional_context: Vec<ContextBlock>,
}

impl Default for PermissionOutput {
    fn default() -> Self {
        Self {
            decision: PermissionDecision::Continue,
            additional_context: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationOutput {
    pub additional_context: Vec<ContextBlock>,
}

/// after_turn 的裁决：接受完成、打回继续（附验证反馈上下文），或直接判负。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterTurnOutput {
    Complete,
    Continue { context: Vec<ContextBlock> },
    Fail { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewareError {
    message: String,
}

impl MiddlewareError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for MiddlewareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MiddlewareError {}

pub type MiddlewareFuture<T> = BoxFuture<'static, Result<T, MiddlewareError>>;

pub trait AgentMiddleware: Send + Sync {
    fn id(&self) -> &str;

    fn source(&self) -> MiddlewareSource {
        MiddlewareSource::Internal
    }

    fn before_tool(&self, _input: BeforeToolInput) -> Option<MiddlewareFuture<GateOutput>> {
        None
    }

    fn permission_request(
        &self,
        _input: PermissionRequestInput,
    ) -> Option<MiddlewareFuture<PermissionOutput>> {
        None
    }

    fn after_tool(&self, _input: AfterToolInput) -> Option<MiddlewareFuture<ObservationOutput>> {
        None
    }

    fn after_turn(&self, _input: AfterTurnInput) -> Option<MiddlewareFuture<AfterTurnOutput>> {
        None
    }
}

#[derive(Clone)]
struct RegisteredAgentMiddleware {
    middleware: Arc<dyn AgentMiddleware>,
    failure_mode: FailureMode,
}

impl RegisteredAgentMiddleware {
    fn metadata(&self) -> MiddlewareMetadata {
        MiddlewareMetadata::new(
            self.middleware.id(),
            self.middleware.source(),
            self.failure_mode,
        )
    }
}

#[derive(Clone, Default)]
pub struct AgentMiddlewareChain {
    entries: Vec<RegisteredAgentMiddleware>,
}

impl std::fmt::Debug for AgentMiddlewareChain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentMiddlewareChain")
            .field("len", &self.entries.len())
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct GateRun {
    pub denied_reasons: Vec<String>,
    pub context: Vec<MiddlewareContextBlock>,
    pub events: Vec<agent_protocol::AgentEvent>,
    pub cancelled: bool,
}

impl GateRun {
    pub fn denied(&self) -> bool {
        !self.denied_reasons.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PermissionRun {
    pub approved_reasons: Vec<String>,
    pub denied_reasons: Vec<String>,
    pub context: Vec<MiddlewareContextBlock>,
    pub events: Vec<agent_protocol::AgentEvent>,
    pub cancelled: bool,
}

impl PermissionRun {
    pub fn decision(&self) -> PermissionDecision {
        if !self.denied_reasons.is_empty() {
            PermissionDecision::Deny {
                reason: self.denied_reasons.join("; "),
            }
        } else if !self.approved_reasons.is_empty() {
            PermissionDecision::Approve {
                reason: Some(self.approved_reasons.join("; ")),
            }
        } else {
            PermissionDecision::Continue
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ObservationRun {
    pub context: Vec<MiddlewareContextBlock>,
    pub events: Vec<agent_protocol::AgentEvent>,
    pub fatal_errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AfterTurnRun {
    pub context: Vec<MiddlewareContextBlock>,
    pub events: Vec<agent_protocol::AgentEvent>,
    /// 判负原因（已带 middleware id 前缀）；非空表示 turn 不得完成。
    pub fail_reasons: Vec<String>,
    /// 任一 middleware 要求打回继续。
    pub continue_requested: bool,
    pub cancelled: bool,
}

impl AgentMiddlewareChain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn register(&mut self, middleware: Arc<dyn AgentMiddleware>) {
        self.register_with_failure_mode(middleware, FailureMode::Closed);
    }

    pub fn register_with_failure_mode(
        &mut self,
        middleware: Arc<dyn AgentMiddleware>,
        failure_mode: FailureMode,
    ) {
        self.entries.push(RegisteredAgentMiddleware {
            middleware,
            failure_mode,
        });
    }

    pub async fn run_before_tool(&self, input: BeforeToolInput) -> GateRun {
        let chain = run_middleware_chain(
            &self.entries,
            &input.context,
            MiddlewareStage::BeforeTool,
            &MIDDLEWARE_INVOCATION_COUNTER,
            RegisteredAgentMiddleware::metadata,
            |entry, context| {
                let mut input = input.clone();
                input.context = context;
                entry.middleware.before_tool(input)
            },
            |_entry, metadata, result, run: &mut GateRun| match result {
                Ok(output) => {
                    let outcome = match &output.decision {
                        GateDecision::Continue => MiddlewareOutcome::Continue,
                        GateDecision::Deny { reason } => {
                            run.denied_reasons.push(attributed_reason(metadata, reason));
                            MiddlewareOutcome::Deny
                        }
                    };
                    let blocks = collect_context(
                        metadata,
                        MiddlewareStage::BeforeTool,
                        output.additional_context,
                    );
                    run.context.extend(blocks.iter().cloned());
                    MiddlewareCompletion::new(outcome, gate_reason(&output.decision))
                        .with_context(blocks)
                }
                Err(error) => {
                    let reason = error.to_string();
                    if metadata.failure_mode == FailureMode::Closed {
                        run.denied_reasons
                            .push(attributed_reason(metadata, &reason));
                    }
                    MiddlewareCompletion::new(metadata.failure_outcome(), Some(reason))
                }
            },
        )
        .await;
        let mut run = chain.aggregate;
        run.events = chain.events;
        run.cancelled = chain.cancelled;
        run
    }

    pub async fn run_permission_request(&self, input: PermissionRequestInput) -> PermissionRun {
        let chain = run_middleware_chain(
            &self.entries,
            &input.context,
            MiddlewareStage::PermissionRequest,
            &MIDDLEWARE_INVOCATION_COUNTER,
            RegisteredAgentMiddleware::metadata,
            |entry, context| {
                let mut input = input.clone();
                input.context = context;
                entry.middleware.permission_request(input)
            },
            |_entry, metadata, result, run: &mut PermissionRun| match result {
                Ok(output) => {
                    let (outcome, reason) = match &output.decision {
                        PermissionDecision::Continue => (MiddlewareOutcome::Continue, None),
                        PermissionDecision::Approve { reason } => {
                            run.approved_reasons.push(attributed_reason(
                                metadata,
                                reason.as_deref().unwrap_or("approved by middleware"),
                            ));
                            (MiddlewareOutcome::Approve, reason.clone())
                        }
                        PermissionDecision::Deny { reason } => {
                            run.denied_reasons.push(attributed_reason(metadata, reason));
                            (MiddlewareOutcome::Deny, Some(reason.clone()))
                        }
                    };
                    let blocks = collect_context(
                        metadata,
                        MiddlewareStage::PermissionRequest,
                        output.additional_context,
                    );
                    run.context.extend(blocks.iter().cloned());
                    MiddlewareCompletion::new(outcome, reason).with_context(blocks)
                }
                Err(error) => {
                    let reason = error.to_string();
                    if metadata.failure_mode == FailureMode::Closed {
                        run.denied_reasons
                            .push(attributed_reason(metadata, &reason));
                    }
                    MiddlewareCompletion::new(metadata.failure_outcome(), Some(reason))
                }
            },
        )
        .await;
        let mut run = chain.aggregate;
        run.events = chain.events;
        run.cancelled = chain.cancelled;
        run
    }

    pub async fn run_after_tool(&self, input: AfterToolInput) -> ObservationRun {
        let chain = run_middleware_chain(
            &self.entries,
            &input.context,
            MiddlewareStage::AfterTool,
            &MIDDLEWARE_INVOCATION_COUNTER,
            RegisteredAgentMiddleware::metadata,
            |entry, context| {
                let mut input = input.clone();
                input.context = context;
                entry.middleware.after_tool(input)
            },
            |_entry, metadata, result, run: &mut ObservationRun| match result {
                Ok(output) => {
                    let blocks = collect_context(
                        metadata,
                        MiddlewareStage::AfterTool,
                        output.additional_context,
                    );
                    run.context.extend(blocks.iter().cloned());
                    MiddlewareCompletion::new(MiddlewareOutcome::Continue, None)
                        .with_context(blocks)
                }
                Err(error) => {
                    let reason = error.to_string();
                    if metadata.failure_mode == FailureMode::Closed {
                        run.fatal_errors.push(attributed_reason(metadata, &reason));
                    }
                    MiddlewareCompletion::new(metadata.failure_outcome(), Some(reason))
                }
            },
        )
        .await;
        let mut run = chain.aggregate;
        run.events = chain.events;
        run.cancelled = chain.cancelled;
        run
    }

    pub async fn run_after_turn(&self, input: AfterTurnInput) -> AfterTurnRun {
        let chain = run_middleware_chain(
            &self.entries,
            &input.context,
            MiddlewareStage::AfterTurn,
            &MIDDLEWARE_INVOCATION_COUNTER,
            RegisteredAgentMiddleware::metadata,
            |entry, context| {
                let mut input = input.clone();
                input.context = context;
                entry.middleware.after_turn(input)
            },
            |_entry, metadata, result, run: &mut AfterTurnRun| match result {
                Ok(output) => match output {
                    AfterTurnOutput::Complete => {
                        MiddlewareCompletion::new(MiddlewareOutcome::Continue, None)
                    }
                    AfterTurnOutput::Continue { context } => {
                        run.continue_requested = true;
                        let blocks = collect_context(metadata, MiddlewareStage::AfterTurn, context);
                        run.context.extend(blocks.iter().cloned());
                        MiddlewareCompletion::new(MiddlewareOutcome::Continue, None)
                            .with_context(blocks)
                    }
                    AfterTurnOutput::Fail { reason } => {
                        run.fail_reasons.push(attributed_reason(metadata, &reason));
                        MiddlewareCompletion::new(MiddlewareOutcome::Deny, Some(reason))
                    }
                },
                Err(error) => {
                    let reason = error.to_string();
                    if metadata.failure_mode == FailureMode::Closed {
                        run.fail_reasons.push(attributed_reason(metadata, &reason));
                    }
                    MiddlewareCompletion::new(metadata.failure_outcome(), Some(reason))
                }
            },
        )
        .await;
        let mut run = chain.aggregate;
        run.events = chain.events;
        run.cancelled = chain.cancelled;
        run
    }
}

fn gate_reason(decision: &GateDecision) -> Option<String> {
    match decision {
        GateDecision::Continue => None,
        GateDecision::Deny { reason } => Some(reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{PermissionMode, ReasoningLevel, ShellPolicy};
    use futures_util::FutureExt;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct TestMiddleware {
        id: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        gate: Result<GateOutput, MiddlewareError>,
        permission: Result<PermissionOutput, MiddlewareError>,
    }

    impl AgentMiddleware for TestMiddleware {
        fn id(&self) -> &str {
            self.id
        }

        fn before_tool(&self, _input: BeforeToolInput) -> Option<MiddlewareFuture<GateOutput>> {
            self.calls.lock().expect("calls").push(self.id);
            let result = self.gate.clone();
            Some(async move { result }.boxed())
        }

        fn permission_request(
            &self,
            _input: PermissionRequestInput,
        ) -> Option<MiddlewareFuture<PermissionOutput>> {
            let result = self.permission.clone();
            Some(async move { result }.boxed())
        }
    }

    fn context() -> MiddlewareExecutionContext {
        MiddlewareExecutionContext {
            invocation_id: None,
            session: "test".to_string(),
            workspace_root: PathBuf::from("/workspace"),
            turn_index: 0,
            operation_id: None,
            turn_id: None,
            model: ModelInvocation {
                provider_id: "test".to_string(),
                provider_name: "Test".to_string(),
                model_id: "model".to_string(),
                model_name: "Model".to_string(),
                reasoning: ReasoningLevel::Off,
            },
            permissions: PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Prompt,
            },
            agent_scope: MiddlewareAgentScope::Main,
            cancellation: CancellationToken::new(),
        }
    }

    fn call() -> ToolCall {
        ToolCall::function("call-1", "shell_command", r#"{"command":"pwd"}"#)
    }

    fn middleware(
        id: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        gate: Result<GateOutput, MiddlewareError>,
        permission: Result<PermissionOutput, MiddlewareError>,
    ) -> Arc<dyn AgentMiddleware> {
        Arc::new(TestMiddleware {
            id,
            calls,
            gate,
            permission,
        })
    }

    #[tokio::test]
    async fn gates_run_in_order_after_deny_and_merge_context() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut chain = AgentMiddlewareChain::new();
        chain.register_with_failure_mode(
            middleware(
                "first",
                calls.clone(),
                Ok(GateOutput {
                    decision: GateDecision::Deny {
                        reason: "blocked".to_string(),
                    },
                    additional_context: vec![ContextBlock::new("one")],
                }),
                Ok(PermissionOutput::default()),
            ),
            FailureMode::Closed,
        );
        chain.register_with_failure_mode(
            middleware(
                "second",
                calls.clone(),
                Ok(GateOutput {
                    decision: GateDecision::Continue,
                    additional_context: vec![ContextBlock::new("two")],
                }),
                Ok(PermissionOutput::default()),
            ),
            FailureMode::Closed,
        );

        let run = chain
            .run_before_tool(BeforeToolInput {
                context: context(),
                tool_call: call(),
            })
            .await;

        assert_eq!(*calls.lock().expect("calls"), vec!["first", "second"]);
        assert!(run.denied());
        assert_eq!(
            run.context
                .iter()
                .map(|block| block.content.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(run.events.len(), 4);
    }

    #[tokio::test]
    async fn permission_deny_wins_over_approve() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut chain = AgentMiddlewareChain::new();
        chain.register_with_failure_mode(
            middleware(
                "approve",
                calls.clone(),
                Ok(GateOutput::default()),
                Ok(PermissionOutput {
                    decision: PermissionDecision::Approve {
                        reason: Some("safe".to_string()),
                    },
                    additional_context: Vec::new(),
                }),
            ),
            FailureMode::Closed,
        );
        chain.register_with_failure_mode(
            middleware(
                "deny",
                calls,
                Ok(GateOutput::default()),
                Ok(PermissionOutput {
                    decision: PermissionDecision::Deny {
                        reason: "policy".to_string(),
                    },
                    additional_context: Vec::new(),
                }),
            ),
            FailureMode::Closed,
        );
        let request = ApprovalRequest::shell_command(
            "approval-call-1",
            "pwd",
            "/workspace",
            10,
            "approval required",
        );

        let run = chain
            .run_permission_request(PermissionRequestInput {
                context: context(),
                tool_call: call(),
                request,
            })
            .await;

        assert!(matches!(run.decision(), PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn failure_mode_controls_gate_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let failing = middleware(
            "broken",
            calls,
            Err(MiddlewareError::new("boom")),
            Ok(PermissionOutput::default()),
        );
        let mut open = AgentMiddlewareChain::new();
        open.register_with_failure_mode(failing.clone(), FailureMode::Open);
        let mut closed = AgentMiddlewareChain::new();
        closed.register(failing);

        let open_run = open
            .run_before_tool(BeforeToolInput {
                context: context(),
                tool_call: call(),
            })
            .await;
        let closed_run = closed
            .run_before_tool(BeforeToolInput {
                context: context(),
                tool_call: call(),
            })
            .await;

        assert!(!open_run.denied());
        assert!(closed_run.denied());
    }

    struct ScriptedAfterTurn {
        id: &'static str,
        outputs: Mutex<Vec<Result<AfterTurnOutput, MiddlewareError>>>,
    }

    impl AgentMiddleware for ScriptedAfterTurn {
        fn id(&self) -> &str {
            self.id
        }

        fn after_turn(&self, input: AfterTurnInput) -> Option<MiddlewareFuture<AfterTurnOutput>> {
            assert_eq!(input.final_text, "done");
            assert_eq!(input.tool_call_count, 1);
            assert_eq!(input.turn_message_count, 3);
            assert_eq!(input.tool_names, vec!["shell_command".to_string()]);
            let output = self.outputs.lock().expect("outputs").remove(0);
            Some(async move { output }.boxed())
        }
    }

    fn after_turn_input() -> AfterTurnInput {
        AfterTurnInput {
            context: context(),
            final_text: "done".to_string(),
            tool_call_count: 1,
            turn_message_count: 3,
            tool_names: vec!["shell_command".to_string()],
        }
    }

    #[tokio::test]
    async fn after_turn_continue_collects_context_and_fail_is_attributed() {
        let mut chain = AgentMiddlewareChain::new();
        chain.register(Arc::new(ScriptedAfterTurn {
            id: "verifier",
            outputs: Mutex::new(vec![Ok(AfterTurnOutput::Continue {
                context: vec![ContextBlock::new("cargo test failed")],
            })]),
        }));
        chain.register(Arc::new(ScriptedAfterTurn {
            id: "lint",
            outputs: Mutex::new(vec![Ok(AfterTurnOutput::Fail {
                reason: "lint errors remain".to_string(),
            })]),
        }));

        let run = chain.run_after_turn(after_turn_input()).await;

        assert!(run.continue_requested);
        assert_eq!(
            run.fail_reasons,
            vec!["lint: lint errors remain".to_string()]
        );
        assert_eq!(
            run.context
                .iter()
                .map(|block| (
                    block.middleware_id.as_str(),
                    block.stage,
                    block.content.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("verifier", MiddlewareStage::AfterTurn, "cargo test failed")]
        );
        assert_eq!(run.events.len(), 4);
        assert!(matches!(
            &run.events[3],
            agent_protocol::AgentEvent::MiddlewareFinished(invocation)
                if invocation.outcome == MiddlewareOutcome::Deny
        ));
    }

    #[tokio::test]
    async fn after_turn_error_fails_closed_or_open_by_failure_mode() {
        let mut closed = AgentMiddlewareChain::new();
        closed.register(Arc::new(ScriptedAfterTurn {
            id: "broken",
            outputs: Mutex::new(vec![Err(MiddlewareError::new("hook crashed"))]),
        }));
        let mut open = AgentMiddlewareChain::new();
        open.register_with_failure_mode(
            Arc::new(ScriptedAfterTurn {
                id: "broken",
                outputs: Mutex::new(vec![Err(MiddlewareError::new("hook crashed"))]),
            }),
            FailureMode::Open,
        );

        let closed_run = closed.run_after_turn(after_turn_input()).await;
        let open_run = open.run_after_turn(after_turn_input()).await;

        assert_eq!(
            closed_run.fail_reasons,
            vec!["broken: hook crashed".to_string()]
        );
        assert!(open_run.fail_reasons.is_empty());
        assert!(!open_run.continue_requested);
    }

    #[tokio::test]
    async fn after_turn_without_implementations_completes_without_events() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut chain = AgentMiddlewareChain::new();
        chain.register(middleware(
            "tool-only",
            calls,
            Ok(GateOutput::default()),
            Ok(PermissionOutput::default()),
        ));

        let run = chain.run_after_turn(after_turn_input()).await;

        assert!(!run.continue_requested);
        assert!(run.fail_reasons.is_empty());
        assert!(run.events.is_empty());
    }
}
