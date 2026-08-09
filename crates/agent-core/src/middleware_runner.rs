use crate::{
    ContextBlock, FailureMode, MiddlewareContextBlock, MiddlewareError, MiddlewareExecutionContext,
    MiddlewareFuture,
};
use agent_protocol::{
    AgentEvent, MiddlewareInvocationFinished, MiddlewareInvocationStarted, MiddlewareOutcome,
    MiddlewareSource, MiddlewareStage,
};
use futures_util::future::{Either, FutureExt, select};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_AUDIT_REASON_CHARS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct MiddlewareMetadata {
    pub id: String,
    pub source: MiddlewareSource,
    pub failure_mode: FailureMode,
}

impl MiddlewareMetadata {
    pub fn new(id: impl Into<String>, source: MiddlewareSource, failure_mode: FailureMode) -> Self {
        Self {
            id: id.into(),
            source,
            failure_mode,
        }
    }

    pub fn failure_outcome(&self) -> MiddlewareOutcome {
        match self.failure_mode {
            FailureMode::Open => MiddlewareOutcome::FailedOpen,
            FailureMode::Closed => MiddlewareOutcome::FailedClosed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MiddlewareCompletion {
    pub outcome: MiddlewareOutcome,
    pub reason: Option<String>,
}

impl MiddlewareCompletion {
    pub fn new(outcome: MiddlewareOutcome, reason: Option<String>) -> Self {
        Self { outcome, reason }
    }
}

#[derive(Debug, Clone)]
pub struct MiddlewareChainRun<T> {
    pub aggregate: T,
    pub events: Vec<AgentEvent>,
    pub cancelled: bool,
}

pub async fn run_middleware_chain<Entry, Output, Aggregate>(
    entries: &[Entry],
    context: &MiddlewareExecutionContext,
    stage: MiddlewareStage,
    invocation_counter: &AtomicU64,
    metadata_for: impl Fn(&Entry) -> MiddlewareMetadata,
    future_for: impl Fn(&Entry, MiddlewareExecutionContext) -> Option<MiddlewareFuture<Output>>,
    mut complete: impl FnMut(
        &Entry,
        &MiddlewareMetadata,
        Result<Output, MiddlewareError>,
        &mut Aggregate,
    ) -> MiddlewareCompletion,
) -> MiddlewareChainRun<Aggregate>
where
    Aggregate: Default,
{
    let mut run = MiddlewareChainRun {
        aggregate: Aggregate::default(),
        events: Vec::new(),
        cancelled: false,
    };

    for entry in entries {
        let metadata = metadata_for(entry);
        let audit = AuditInvocation::start(&metadata, stage, invocation_counter);
        let mut invocation_context = context.clone();
        invocation_context.invocation_id = Some(audit.invocation_id.clone());
        let Some(future) = future_for(entry, invocation_context) else {
            continue;
        };

        run.events.push(audit.started_event());
        match await_middleware(future, context).await {
            MiddlewareCall::Completed(result) => {
                let completion = complete(entry, &metadata, result, &mut run.aggregate);
                run.events
                    .push(audit.finished_event(completion.outcome, completion.reason));
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

pub fn append_context(
    target: &mut Vec<MiddlewareContextBlock>,
    metadata: &MiddlewareMetadata,
    stage: MiddlewareStage,
    blocks: Vec<ContextBlock>,
) {
    target.extend(blocks.into_iter().filter_map(|block| {
        let content = block.content.trim().to_string();
        (!content.is_empty()).then(|| MiddlewareContextBlock {
            middleware_id: metadata.id.clone(),
            source: metadata.source,
            stage,
            content,
        })
    }));
}

pub fn attributed_reason(metadata: &MiddlewareMetadata, reason: &str) -> String {
    format!("{}: {reason}", metadata.id)
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

struct AuditInvocation {
    invocation_id: String,
    middleware_id: String,
    source: MiddlewareSource,
    stage: MiddlewareStage,
    started_at_ms: u64,
    started: Instant,
}

impl AuditInvocation {
    fn start(
        metadata: &MiddlewareMetadata,
        stage: MiddlewareStage,
        invocation_counter: &AtomicU64,
    ) -> Self {
        Self {
            invocation_id: next_invocation_id(invocation_counter),
            middleware_id: metadata.id.clone(),
            source: metadata.source,
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

fn next_invocation_id(invocation_counter: &AtomicU64) -> String {
    let counter = invocation_counter.fetch_add(1, Ordering::Relaxed);
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
