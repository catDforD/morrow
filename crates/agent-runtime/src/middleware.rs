use agent_core::{
    ContextBlock, FailureMode, GateDecision, GateOutput, GateRun, MiddlewareContextBlock,
    MiddlewareError, MiddlewareExecutionContext, MiddlewareFuture, ObservationOutput,
    ObservationRun,
};
use agent_protocol::{
    AgentEvent, MiddlewareInvocationFinished, MiddlewareInvocationStarted, MiddlewareOutcome,
    MiddlewareSource, MiddlewareStage,
};
use futures_util::future::{Either, FutureExt, select};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_AUDIT_REASON_CHARS: usize = 4_096;
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
            |middleware| middleware.before_prompt(input.clone()),
        )
        .await
    }

    pub async fn run_pre_compact(&self, input: PreCompactInput) -> GateRun {
        self.run_gate(
            MiddlewareStage::PreCompact,
            input.context.clone(),
            |middleware| middleware.pre_compact(input.clone()),
        )
        .await
    }

    pub async fn run_post_compact(&self, input: PostCompactInput) -> ObservationRun {
        let mut run = ObservationRun::default();
        for entry in &self.entries {
            let Some(future) = entry.middleware.post_compact(input.clone()) else {
                continue;
            };
            let audit = AuditInvocation::start(
                entry.middleware.id(),
                entry.middleware.source(),
                MiddlewareStage::PostCompact,
            );
            run.events.push(audit.started_event());
            match await_middleware(future, &input.context).await {
                MiddlewareCall::Completed(Ok(output)) => {
                    append_context(
                        &mut run.context,
                        entry.middleware.as_ref(),
                        MiddlewareStage::PostCompact,
                        output.additional_context,
                    );
                    run.events
                        .push(audit.finished_event(MiddlewareOutcome::Continue, None));
                }
                MiddlewareCall::Completed(Err(error)) => {
                    let reason = error.to_string();
                    let outcome = match entry.failure_mode {
                        FailureMode::Open => MiddlewareOutcome::FailedOpen,
                        FailureMode::Closed => {
                            run.fatal_errors
                                .push(format!("{}: {reason}", entry.middleware.id()));
                            MiddlewareOutcome::FailedClosed
                        }
                    };
                    run.events.push(audit.finished_event(outcome, Some(reason)));
                }
                MiddlewareCall::Cancelled => {
                    run.cancelled = true;
                    run.events.push(audit.finished_event(
                        MiddlewareOutcome::Cancelled,
                        Some("operation cancelled".to_string()),
                    ));
                    break;
                }
            }
        }
        run
    }

    async fn run_gate(
        &self,
        stage: MiddlewareStage,
        context: MiddlewareExecutionContext,
        future_for: impl Fn(&dyn RuntimeMiddleware) -> Option<MiddlewareFuture<GateOutput>>,
    ) -> GateRun {
        let mut run = GateRun::default();
        for entry in &self.entries {
            let Some(future) = future_for(entry.middleware.as_ref()) else {
                continue;
            };
            let audit =
                AuditInvocation::start(entry.middleware.id(), entry.middleware.source(), stage);
            run.events.push(audit.started_event());
            match await_middleware(future, &context).await {
                MiddlewareCall::Completed(Ok(output)) => {
                    let (outcome, reason) = match &output.decision {
                        GateDecision::Continue => (MiddlewareOutcome::Continue, None),
                        GateDecision::Deny { reason } => {
                            run.denied_reasons
                                .push(format!("{}: {reason}", entry.middleware.id()));
                            (MiddlewareOutcome::Deny, Some(reason.clone()))
                        }
                    };
                    append_context(
                        &mut run.context,
                        entry.middleware.as_ref(),
                        stage,
                        output.additional_context,
                    );
                    run.events.push(audit.finished_event(outcome, reason));
                }
                MiddlewareCall::Completed(Err(error)) => {
                    let reason = error.to_string();
                    let outcome = match entry.failure_mode {
                        FailureMode::Open => MiddlewareOutcome::FailedOpen,
                        FailureMode::Closed => {
                            run.denied_reasons
                                .push(format!("{}: {reason}", entry.middleware.id()));
                            MiddlewareOutcome::FailedClosed
                        }
                    };
                    run.events.push(audit.finished_event(outcome, Some(reason)));
                }
                MiddlewareCall::Cancelled => {
                    run.cancelled = true;
                    run.events.push(audit.finished_event(
                        MiddlewareOutcome::Cancelled,
                        Some("operation cancelled".to_string()),
                    ));
                    break;
                }
            }
        }
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

enum MiddlewareCall<T> {
    Completed(Result<T, MiddlewareError>),
    Cancelled,
}

async fn await_middleware<T>(
    future: MiddlewareFuture<T>,
    context: &MiddlewareExecutionContext,
) -> MiddlewareCall<T> {
    let cancellation = context.cancellation.clone();
    let cancelled = async move { cancellation.cancelled().await }.boxed();
    match select(future, cancelled).await {
        Either::Left((result, _)) => MiddlewareCall::Completed(result),
        Either::Right(((), _)) => MiddlewareCall::Cancelled,
    }
}

fn append_context(
    target: &mut Vec<MiddlewareContextBlock>,
    middleware: &dyn RuntimeMiddleware,
    stage: MiddlewareStage,
    blocks: Vec<ContextBlock>,
) {
    target.extend(blocks.into_iter().filter_map(|block| {
        let content = block.content.trim().to_string();
        (!content.is_empty()).then(|| MiddlewareContextBlock {
            middleware_id: middleware.id().to_string(),
            source: middleware.source(),
            stage,
            content,
        })
    }));
}

struct AuditInvocation {
    invocation_id: String,
    middleware_id: String,
    source: MiddlewareSource,
    stage: MiddlewareStage,
    started_at_ms: u64,
    started: Instant,
}

impl AuditInvocation {
    fn start(id: &str, source: MiddlewareSource, stage: MiddlewareStage) -> Self {
        Self {
            invocation_id: next_invocation_id(),
            middleware_id: id.to_string(),
            source,
            stage,
            started_at_ms: timestamp_ms(),
            started: Instant::now(),
        }
    }

    fn started_event(&self) -> AgentEvent {
        AgentEvent::MiddlewareStarted(MiddlewareInvocationStarted {
            invocation_id: self.invocation_id.clone(),
            middleware_id: self.middleware_id.clone(),
            source: self.source,
            stage: self.stage,
            started_at_ms: self.started_at_ms,
        })
    }

    fn finished_event(&self, outcome: MiddlewareOutcome, reason: Option<String>) -> AgentEvent {
        AgentEvent::MiddlewareFinished(MiddlewareInvocationFinished {
            invocation_id: self.invocation_id.clone(),
            middleware_id: self.middleware_id.clone(),
            source: self.source,
            stage: self.stage,
            outcome,
            started_at_ms: self.started_at_ms,
            duration_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            reason: reason.map(|reason| truncate_chars(reason, MAX_AUDIT_REASON_CHARS)),
        })
    }
}

fn next_invocation_id() -> String {
    let counter = RUNTIME_MIDDLEWARE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("middleware-{:016x}-{counter:04x}", timestamp_ms())
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value
    } else {
        value.chars().take(max_chars).collect()
    }
}
