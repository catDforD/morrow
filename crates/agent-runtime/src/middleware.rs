use agent_core::middleware_runner::{
    MiddlewareCompletion, MiddlewareMetadata, attributed_reason, collect_context,
    run_middleware_chain,
};
use agent_core::{
    FailureMode, GateDecision, GateOutput, GateRun, MiddlewareExecutionContext, MiddlewareFuture,
    ObservationOutput, ObservationRun,
};
use agent_protocol::{MiddlewareOutcome, MiddlewareSource, MiddlewareStage};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

static RUNTIME_MIDDLEWARE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct BeforePromptInput {
    pub context: MiddlewareExecutionContext,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionCause {
    Automatic,
    Manual,
}

#[derive(Debug, Clone)]
pub struct PreCompactInput {
    pub context: MiddlewareExecutionContext,
    pub cause: CompactionCause,
    pub estimated_tokens: usize,
    pub token_budget: Option<usize>,
    pub current_summary: Option<String>,
    pub summarized_turns: usize,
}

#[derive(Debug, Clone)]
pub struct PostCompactInput {
    pub context: MiddlewareExecutionContext,
    pub cause: CompactionCause,
    pub previous_summary: Option<String>,
    pub summary: String,
    pub summarized_turns: usize,
}

pub trait RuntimeMiddleware: Send + Sync {
    fn id(&self) -> &str;

    fn source(&self) -> MiddlewareSource {
        MiddlewareSource::Internal
    }

    fn before_prompt(&self, _input: BeforePromptInput) -> Option<MiddlewareFuture<GateOutput>> {
        None
    }

    fn pre_compact(&self, _input: PreCompactInput) -> Option<MiddlewareFuture<GateOutput>> {
        None
    }

    fn post_compact(
        &self,
        _input: PostCompactInput,
    ) -> Option<MiddlewareFuture<ObservationOutput>> {
        None
    }
}

#[derive(Clone)]
struct RegisteredRuntimeMiddleware {
    middleware: Arc<dyn RuntimeMiddleware>,
    failure_mode: FailureMode,
}

impl RegisteredRuntimeMiddleware {
    fn metadata(&self) -> MiddlewareMetadata {
        MiddlewareMetadata::new(
            self.middleware.id(),
            self.middleware.source(),
            self.failure_mode,
        )
    }
}

#[derive(Clone, Default)]
pub struct RuntimeMiddlewareChain {
    entries: Vec<RegisteredRuntimeMiddleware>,
}

impl std::fmt::Debug for RuntimeMiddlewareChain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeMiddlewareChain")
            .field("len", &self.entries.len())
            .finish()
    }
}

impl RuntimeMiddlewareChain {
    pub fn register(&mut self, middleware: Arc<dyn RuntimeMiddleware>) {
        self.register_with_failure_mode(middleware, FailureMode::Closed);
    }

    pub fn register_with_failure_mode(
        &mut self,
        middleware: Arc<dyn RuntimeMiddleware>,
        failure_mode: FailureMode,
    ) {
        self.entries.push(RegisteredRuntimeMiddleware {
            middleware,
            failure_mode,
        });
    }

    pub async fn run_before_prompt(&self, input: BeforePromptInput) -> GateRun {
        self.run_gate(
            MiddlewareStage::BeforePrompt,
            input.context.clone(),
            |middleware, context| {
                let mut input = input.clone();
                input.context = context;
                middleware.before_prompt(input)
            },
        )
        .await
    }

    pub async fn run_pre_compact(&self, input: PreCompactInput) -> GateRun {
        self.run_gate(
            MiddlewareStage::PreCompact,
            input.context.clone(),
            |middleware, context| {
                let mut input = input.clone();
                input.context = context;
                middleware.pre_compact(input)
            },
        )
        .await
    }

    pub async fn run_post_compact(&self, input: PostCompactInput) -> ObservationRun {
        let chain = run_middleware_chain(
            &self.entries,
            &input.context,
            MiddlewareStage::PostCompact,
            &RUNTIME_MIDDLEWARE_COUNTER,
            RegisteredRuntimeMiddleware::metadata,
            |entry, context| {
                let mut input = input.clone();
                input.context = context;
                entry.middleware.post_compact(input)
            },
            |_entry, metadata, result, run: &mut ObservationRun| match result {
                Ok(output) => {
                    let blocks = collect_context(
                        metadata,
                        MiddlewareStage::PostCompact,
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

    async fn run_gate(
        &self,
        stage: MiddlewareStage,
        context: MiddlewareExecutionContext,
        future_for: impl Fn(
            &dyn RuntimeMiddleware,
            MiddlewareExecutionContext,
        ) -> Option<MiddlewareFuture<GateOutput>>,
    ) -> GateRun {
        let chain = run_middleware_chain(
            &self.entries,
            &context,
            stage,
            &RUNTIME_MIDDLEWARE_COUNTER,
            RegisteredRuntimeMiddleware::metadata,
            |entry, context| future_for(entry.middleware.as_ref(), context),
            |_entry, metadata, result, run: &mut GateRun| match result {
                Ok(output) => {
                    let (outcome, reason) = match &output.decision {
                        GateDecision::Continue => (MiddlewareOutcome::Continue, None),
                        GateDecision::Deny { reason } => {
                            run.denied_reasons.push(attributed_reason(metadata, reason));
                            (MiddlewareOutcome::Deny, Some(reason.clone()))
                        }
                    };
                    let blocks = collect_context(metadata, stage, output.additional_context);
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
}

#[derive(Clone, Default)]
pub struct MiddlewareRegistry {
    agent: agent_core::AgentMiddlewareChain,
    runtime: RuntimeMiddlewareChain,
}

impl std::fmt::Debug for MiddlewareRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MiddlewareRegistry")
            .field("agent", &self.agent)
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl MiddlewareRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn agent(&self) -> &agent_core::AgentMiddlewareChain {
        &self.agent
    }

    pub fn runtime(&self) -> &RuntimeMiddlewareChain {
        &self.runtime
    }

    pub fn register_agent(&mut self, middleware: Arc<dyn agent_core::AgentMiddleware>) {
        self.agent.register(middleware);
    }

    pub fn register_agent_with_failure_mode(
        &mut self,
        middleware: Arc<dyn agent_core::AgentMiddleware>,
        failure_mode: FailureMode,
    ) {
        self.agent
            .register_with_failure_mode(middleware, failure_mode);
    }

    pub fn register_runtime(&mut self, middleware: Arc<dyn RuntimeMiddleware>) {
        self.runtime.register(middleware);
    }

    pub fn register_runtime_with_failure_mode(
        &mut self,
        middleware: Arc<dyn RuntimeMiddleware>,
        failure_mode: FailureMode,
    ) {
        self.runtime
            .register_with_failure_mode(middleware, failure_mode);
    }
}
