pub mod session_store;

use agent_config::{ContextConfig, McpServerConfig, ModelContextLimits};
use agent_core::{
    Agent, AgentError, Model, ModelEvent, ModelFailure, ModelRequest, ToolExecutionContext,
};
use agent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Conversation, Message, ModelInvocation,
    PermissionProfile, Session, SubagentExecutionSummary, Thread, ToolDefinition, TurnRecord,
    TurnStatus, TurnStepKind,
};
use agent_tools::{SubagentExecutor, ToolRegistry, ToolRegistryError};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use agent_core::CancellationToken;
pub use agent_tools::McpToolCache;
pub use session_store::{SessionEntry, SessionListingEntry, SessionStore, SessionStoreError};

pub const EVENT_SCHEMA_VERSION: u32 = 4;
const MESSAGE_BASE_TOKENS: usize = 6;
const TOOL_CALL_BASE_TOKENS: usize = 12;
const REQUEST_PADDING_NUMERATOR: usize = 4;
const REQUEST_PADDING_DENOMINATOR: usize = 3;
const REQUIRED_SUMMARY_SECTIONS: [&str; 7] = [
    "User Goals and Constraints",
    "Important Decisions",
    "Files and Code State",
    "Commands, Results, and Errors",
    "Current Progress",
    "Pending Tasks",
    "Open Questions",
];
const MAX_SUBAGENTS_PER_TURN: usize = 4;
const SUBAGENT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SUBAGENT_RESULT_CHARS: usize = 12_000;
const PARENT_SUBAGENT_GUIDANCE: &str = "You may delegate up to four independent, read-only workspace investigations with delegate_task. Each delegated task must be self-contained. Issue multiple delegate_task calls in the same response when the investigations can run in parallel, and use direct tools for simple lookups.";
const CHILD_SUBAGENT_GUIDANCE: &str = "You are a read-only research subagent working for another coding agent. Complete only the delegated task. Inspect the workspace with read_file, list_files, and search_text. Do not modify files, run commands, call external services, or delegate further. Return a concise, evidence-based report with relevant file paths or symbols and any unresolved uncertainty.";

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Model(#[from] ModelFailure),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Tools(#[from] ToolRegistryError),
    #[error(transparent)]
    SessionStore(#[from] SessionStoreError),
    #[error("agent run failed: {0}")]
    AgentRun(String),
    #[error("turn event handler failed: {0}")]
    EventHandler(String),
}

impl RuntimeError {
    pub fn event_handler(error: impl ToString) -> Self {
        Self::EventHandler(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    Changed,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    pub schema_version: u32,
    pub timestamp_ms: u64,
    pub session: String,
    pub workspace_root: String,
    pub turn_index: usize,
    pub event_index: usize,
    pub event: AgentEvent,
}

#[derive(Clone, Copy)]
pub struct RunAgentTurnContext<'a> {
    pub client: &'a dyn Model,
    pub model: &'a ModelInvocation,
    pub system_prompt: &'a str,
    pub context_config: ContextConfig,
    pub model_limits: ModelContextLimits,
    pub workspace_root: &'a Path,
    pub permissions: PermissionProfile,
    pub mcp_servers: &'a [McpServerConfig],
    pub mcp_cache: &'a McpToolCache,
    pub session_name: &'a str,
    pub turn_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentTurnOutcome {
    /// 表示调用方持有的 Session 已被更新，应执行持久化。
    pub session_changed: bool,
    /// agent 或事件接收方错误。事件投递可能在 turn 完成后失败，因此这里为 Some
    /// 不等于 `TurnStatus::Failed`；最终状态应以 Session 中的 TurnRecord 为准。
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpInspectionTool {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpInspection {
    pub tools: Vec<McpInspectionTool>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
struct RuntimeSubagentExecutor {
    model: Arc<dyn Model>,
    system_prompt: Arc<str>,
    workspace_root: Arc<PathBuf>,
    started: Arc<AtomicUsize>,
    timeout: Duration,
    max_result_chars: usize,
}

impl RuntimeSubagentExecutor {
    fn new(
        model: Arc<dyn Model>,
        system_prompt: impl Into<Arc<str>>,
        workspace_root: impl Into<Arc<PathBuf>>,
    ) -> Self {
        Self {
            model,
            system_prompt: system_prompt.into(),
            workspace_root: workspace_root.into(),
            started: Arc::new(AtomicUsize::new(0)),
            timeout: SUBAGENT_TIMEOUT,
            max_result_chars: MAX_SUBAGENT_RESULT_CHARS,
        }
    }

    async fn execute_inner(
        self,
        task: String,
        parent_cancellation: CancellationToken,
    ) -> SubagentExecutionSummary {
        if self
            .started
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |started| {
                (started < MAX_SUBAGENTS_PER_TURN).then_some(started + 1)
            })
            .is_err()
        {
            return SubagentExecutionSummary::failure(
                task,
                format!("subagent limit exceeded ({MAX_SUBAGENTS_PER_TURN} per turn)"),
                0,
                0,
            );
        }

        let child_cancellation = CancellationToken::new();
        let run = self.run_task(task.clone(), child_cancellation.clone());
        tokio::pin!(run);

        tokio::select! {
            biased;
            _ = parent_cancellation.cancelled() => {
                child_cancellation.cancel();
                let summary = run.await;
                fail_subagent_summary(summary, "subagent execution cancelled")
            }
            _ = tokio::time::sleep(self.timeout) => {
                child_cancellation.cancel();
                let summary = run.await;
                fail_subagent_summary(
                    summary,
                    format!("subagent timed out after {} seconds", self.timeout.as_secs()),
                )
            }
            summary = &mut run => summary,
        }
    }

    async fn run_task(
        &self,
        task: String,
        cancellation: CancellationToken,
    ) -> SubagentExecutionSummary {
        let tools = match ToolRegistry::research(self.workspace_root.as_ref()) {
            Ok(tools) => tools,
            Err(error) => {
                return SubagentExecutionSummary::failure(task, error.to_string(), 0, 0);
            }
        };
        let system_prompt = format!("{}\n\n{CHILD_SUBAGENT_GUIDANCE}", self.system_prompt);
        let agent = Agent::with_tools(self.model.as_ref(), system_prompt, &tools);
        let mut stream = match agent
            .run_turn_with_context(
                &Thread::new(),
                task.clone(),
                ToolExecutionContext {
                    cancellation: cancellation.clone(),
                },
            )
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                return SubagentExecutionSummary::failure(task, error.to_string(), 0, 0);
            }
        };

        let mut cancellation_observed = false;
        loop {
            let event = if cancellation_observed {
                stream.next().await
            } else {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        stream.cancel();
                        cancellation_observed = true;
                        continue;
                    }
                    event = stream.next() => event,
                }
            };
            let Some(event) = event else {
                break;
            };
            if let AgentEvent::ApprovalRequested(request) = event
                && let Err(error) = stream.resolve_approval(ApprovalDecision::deny(request.id))
            {
                stream.cancel_with_reason(error);
                cancellation_observed = true;
            }
        }

        let record = stream.into_turn_record();
        let model_calls = record
            .turn
            .steps
            .iter()
            .filter(|step| step.kind == TurnStepKind::ModelCall)
            .count();
        let tool_calls = record
            .turn
            .steps
            .iter()
            .filter(|step| step.kind == TurnStepKind::ToolCall)
            .count();
        if record.turn.status != TurnStatus::Completed {
            return SubagentExecutionSummary::failure(
                task,
                record
                    .turn
                    .error
                    .unwrap_or_else(|| "subagent turn failed".to_string()),
                model_calls,
                tool_calls,
            );
        }

        let Some(result) = record
            .turn
            .assistant_message
            .and_then(|message| message.content)
            .filter(|result| !result.trim().is_empty())
        else {
            return SubagentExecutionSummary::failure(
                task,
                "subagent returned an empty result",
                model_calls,
                tool_calls,
            );
        };
        let (result, truncated) = truncate_chars(result, self.max_result_chars);
        SubagentExecutionSummary::success(task, result, model_calls, tool_calls, truncated)
    }
}

impl SubagentExecutor for RuntimeSubagentExecutor {
    fn execute(
        &self,
        task: String,
        cancellation: CancellationToken,
    ) -> BoxFuture<'static, SubagentExecutionSummary> {
        let executor = self.clone();
        async move { executor.execute_inner(task, cancellation).await }.boxed()
    }
}

fn fail_subagent_summary(
    mut summary: SubagentExecutionSummary,
    error: impl Into<String>,
) -> SubagentExecutionSummary {
    summary.result = None;
    summary.error = Some(error.into());
    summary.truncated = false;
    summary
}

fn truncate_chars(value: String, max_chars: usize) -> (String, bool) {
    if value.chars().count() <= max_chars {
        return (value, false);
    }
    (value.chars().take(max_chars).collect(), true)
}

pub async fn inspect_mcp_servers(
    workspace_root: &Path,
    servers: &[McpServerConfig],
) -> McpInspection {
    let cache = McpToolCache::new();
    let discovery = agent_tools::mcp::discover_tools(workspace_root, servers, &cache).await;
    let mut tools = discovery
        .tools
        .into_iter()
        .flat_map(|provider| provider.definitions())
        .map(|definition| McpInspectionTool {
            name: definition.function.name,
            description: definition.function.description,
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    cache.clear().await;

    McpInspection {
        tools,
        diagnostics: discovery.diagnostics,
    }
}

pub trait TurnEventHandler {
    fn on_event(&mut self, _event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, RuntimeError>> {
        async move { Ok(ApprovalDecision::deny(request.id.clone())) }.boxed()
    }
}

pub async fn run_agent_turn(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    run_agent_turn_with_cancellation(context, session, prompt, handler, CancellationToken::new())
        .await
}

pub async fn run_agent_turn_with_cancellation(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    // 所有状态先写入草稿；只有整个用例正常收束后才替换调用方持有的 Session。
    let mut draft = session.clone();
    let outcome = run_agent_turn_inner(context, &mut draft, prompt, handler, &cancellation).await?;
    *session = draft;
    Ok(outcome)
}

async fn run_agent_turn_inner(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: &CancellationToken,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let build = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = ToolRegistry::with_mcp_cache_async(
            context.workspace_root,
            context.permissions,
            context.mcp_servers,
            context.mcp_cache,
        ) => Some(result),
    };
    let Some(build) = build else {
        return Ok(record_cancelled_turn(session, prompt, context.model));
    };
    let build = build?;
    let mut tools = build.registry;
    let diagnostics = build.diagnostics;
    let effective_system_prompt = if let Some(model) = context.client.shared_clone() {
        tools.register_subagent(Arc::new(RuntimeSubagentExecutor::new(
            model,
            Arc::<str>::from(context.system_prompt),
            Arc::new(context.workspace_root.to_path_buf()),
        )))?;
        format!("{}\n\n{PARENT_SUBAGENT_GUIDANCE}", context.system_prompt)
    } else {
        context.system_prompt.to_string()
    };
    let tool_definitions = tools.definitions();

    let compaction = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = maybe_auto_compact_with_tools(
            context.client,
            &effective_system_prompt,
            session,
            context.context_config,
            context.model_limits,
            prompt,
            &tool_definitions,
        ) => Some(result),
    };
    let Some(compaction) = compaction else {
        return Ok(record_cancelled_turn(session, prompt, context.model));
    };
    if let Err(error) = compaction {
        let message = format!("context compaction failed: {error}");
        apply_turn_with_model(
            session,
            TurnRecord::failed_user_prompt(prompt, message.clone()),
            context.model,
        );
        return Ok(RunAgentTurnOutcome {
            session_changed: true,
            error: Some(message),
        });
    }

    let agent = Agent::with_tools(context.client, effective_system_prompt, &tools);
    let mut agent_error = None;
    let mut handler_error = None;
    let mut turn_completed = false;
    let mut event_index = 0;

    for diagnostic in diagnostics {
        let envelope = make_event_envelope(
            context.session_name,
            context.workspace_root,
            context.turn_index,
            event_index,
            AgentEvent::Warning(diagnostic),
        );
        event_index += 1;
        if let Err(error) = handler.on_event(&envelope) {
            return Ok(record_failed_turn(
                session,
                prompt,
                context.model,
                error.to_string(),
            ));
        }
    }

    {
        let mut stream = agent
            .run_turn_with_context(
                &session.active_thread,
                prompt.to_string(),
                ToolExecutionContext {
                    cancellation: cancellation.clone(),
                },
            )
            .await?;
        stream.set_model_invocation(context.model.clone());

        let mut cancellation_observed = false;
        loop {
            let event = if cancellation_observed {
                stream.next().await
            } else {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        stream.cancel();
                        cancellation_observed = true;
                        continue;
                    },
                    event = stream.next() => event,
                }
            };
            let Some(event) = event else {
                break;
            };
            let envelope = make_event_envelope(
                context.session_name,
                context.workspace_root,
                context.turn_index,
                event_index,
                event.clone(),
            );
            event_index += 1;
            match &event {
                AgentEvent::TurnCompleted => {
                    turn_completed = true;
                }
                AgentEvent::Error(message) => {
                    agent_error = Some(message.clone());
                }
                AgentEvent::TurnStarted
                | AgentEvent::Warning(_)
                | AgentEvent::ReasoningDelta(_)
                | AgentEvent::TextDelta(_)
                | AgentEvent::AgentMessage(_)
                | AgentEvent::SubagentStarted { .. }
                | AgentEvent::SubagentFinished { .. }
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolCallFinished { .. }
                | AgentEvent::ApprovalRequested(_)
                | AgentEvent::ApprovalResolved(_) => {}
            }

            if handler_error.is_none()
                && let Err(error) = handler.on_event(&envelope)
            {
                let error = error.to_string();
                handler_error = Some(error.clone());
                stream.cancel_with_reason(error);
                cancellation_observed = true;
                continue;
            }

            if let AgentEvent::ApprovalRequested(request) = event {
                let decision = if cancellation_observed {
                    ApprovalDecision::deny(request.id.clone())
                } else {
                    let result = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            stream.cancel();
                            cancellation_observed = true;
                            continue;
                        },
                        result = handler.resolve_approval(&request) => result,
                    };
                    match result {
                        Ok(decision) => decision,
                        Err(error) => {
                            let error = error.to_string();
                            handler_error = Some(error.clone());
                            stream.cancel_with_reason(error);
                            cancellation_observed = true;
                            continue;
                        }
                    }
                };
                stream.resolve_approval(decision)?;
            }
        }

        apply_turn_with_model(session, stream.into_turn_record(), context.model);
    }

    Ok(RunAgentTurnOutcome {
        session_changed: true,
        error: handler_error.or_else(|| agent_error.filter(|_| !turn_completed)),
    })
}

fn apply_turn_with_model(session: &mut Session, mut record: TurnRecord, model: &ModelInvocation) {
    if record.turn.model.is_none() {
        record.turn.model = Some(model.clone());
    }
    session.apply_turn(record);
}

fn record_cancelled_turn(
    session: &mut Session,
    prompt: &str,
    model: &ModelInvocation,
) -> RunAgentTurnOutcome {
    record_failed_turn(session, prompt, model, "turn cancelled")
}

fn record_failed_turn(
    session: &mut Session,
    prompt: &str,
    model: &ModelInvocation,
    message: impl Into<String>,
) -> RunAgentTurnOutcome {
    let message = message.into();
    apply_turn_with_model(
        session,
        TurnRecord::failed_user_prompt(prompt, message.clone()),
        model,
    );
    RunAgentTurnOutcome {
        session_changed: true,
        error: Some(message),
    }
}

pub fn make_event_envelope(
    session_name: &str,
    workspace_root: &Path,
    turn_index: usize,
    event_index: usize,
    event: AgentEvent,
) -> AgentEventEnvelope {
    AgentEventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_ms: timestamp_ms(),
        session: session_name.to_string(),
        workspace_root: workspace_root.display().to_string(),
        turn_index,
        event_index,
        event,
    }
}

pub fn timestamp_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

pub async fn maybe_auto_compact(
    client: &dyn Model,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    prompt: &str,
) -> Result<(), RuntimeError> {
    maybe_auto_compact_with_tools(
        client,
        system_prompt,
        session,
        context_config,
        model_limits,
        prompt,
        &[],
    )
    .await
}

pub async fn maybe_auto_compact_with_tools(
    client: &dyn Model,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    prompt: &str,
    tools: &[ToolDefinition],
) -> Result<(), RuntimeError> {
    if !context_config.auto_compact {
        return Ok(());
    }

    let budget = auto_compact_trigger_tokens(model_limits, context_config);
    let estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if estimate <= budget {
        return Ok(());
    }

    compact_session(client, session, context_config).await?;

    let compacted_estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if compacted_estimate > budget {
        return Err(RuntimeError::AgentRun(format!(
            "context is still over token budget after compaction ({compacted_estimate} > {budget})"
        )));
    }

    Ok(())
}

fn auto_compact_trigger_tokens(
    model_limits: ModelContextLimits,
    context_config: ContextConfig,
) -> usize {
    let input_window = model_limits
        .context_window_tokens
        .saturating_sub(model_limits.reserved_output_tokens);
    ((input_window as f64) * f64::from(context_config.auto_compact_threshold)).floor() as usize
}

pub async fn compact_session(
    client: &dyn Model,
    session: &mut Session,
    context_config: ContextConfig,
) -> Result<CompactionOutcome, RuntimeError> {
    let prefix_len = compactable_prefix_len(session, context_config.retain_recent_turns);
    if prefix_len <= session.context.summarized_turns {
        return Ok(CompactionOutcome::Noop);
    }

    let records = session.turns[session.context.summarized_turns..prefix_len].to_vec();
    let summary = request_session_summary(
        client,
        session.context.summary.as_deref(),
        context_config.summary_target_tokens,
        context_config.compact_max_retries,
        &records,
        session.context.summarized_turns,
    )
    .await?;

    session.context.summary = Some(summary);
    session.context.summarized_turns = prefix_len;
    rebuild_active_thread(session);

    Ok(CompactionOutcome::Changed)
}

pub fn rebuild_active_thread(session: &mut Session) {
    let mut messages = Vec::new();
    if let Some(summary) = session.context.summary.as_ref() {
        messages.push(Message::system(format!("Session summary:\n{summary}")));
    }

    for record in session.turns.iter().skip(session.context.summarized_turns) {
        if record.turn.status == TurnStatus::Completed {
            messages.extend(record.messages.clone());
        }
    }

    session.active_thread.messages = messages;
}

pub fn detect_workspace_root() -> Result<PathBuf, RuntimeError> {
    let cwd = std::env::current_dir().map_err(SessionStoreError::CurrentDir)?;
    let mut candidate = cwd.as_path();

    loop {
        let manifest = candidate.join("Cargo.toml");
        if manifest.is_file() && manifest_has_workspace_header(&manifest) {
            return Ok(candidate.to_path_buf());
        }
        let Some(parent) = candidate.parent() else {
            return Ok(cwd);
        };
        candidate = parent;
    }
}

fn compactable_prefix_len(session: &Session, retain_recent_turns: usize) -> usize {
    let completed_indices = session
        .turns
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            (record.turn.status == TurnStatus::Completed).then_some(index)
        })
        .collect::<Vec<_>>();

    if completed_indices.len() <= retain_recent_turns {
        return session.context.summarized_turns;
    }

    completed_indices[completed_indices.len() - retain_recent_turns]
        .max(session.context.summarized_turns)
}

async fn request_session_summary(
    client: &dyn Model,
    existing_summary: Option<&str>,
    target_tokens: usize,
    max_attempts: usize,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> Result<String, RuntimeError> {
    let attempts = max_attempts.max(1);
    let mut repair_feedback = None;

    for _ in 0..attempts {
        let output = match request_raw_session_summary(
            client,
            existing_summary,
            target_tokens,
            repair_feedback.as_deref(),
            records,
            first_turn_index,
        )
        .await
        {
            Ok(output) => output,
            Err(_) => {
                return Ok(deterministic_session_summary(
                    existing_summary,
                    records,
                    first_turn_index,
                ));
            }
        };

        match parse_compact_summary_output(&output) {
            Ok(summary) => return Ok(summary),
            Err(error) => {
                repair_feedback = Some(error);
            }
        }
    }

    Ok(deterministic_session_summary(
        existing_summary,
        records,
        first_turn_index,
    ))
}

async fn request_raw_session_summary(
    client: &dyn Model,
    existing_summary: Option<&str>,
    target_tokens: usize,
    repair_feedback: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> Result<String, RuntimeError> {
    let mut conversation = Conversation::with_system_prompt(
        "You compact long-running coding agent session history. Respond with text only. Do not call tools. Return one <analysis> block followed by one <summary> block.",
    );
    conversation.push(Message::user(build_summary_prompt(
        existing_summary,
        target_tokens,
        repair_feedback,
        records,
        first_turn_index,
    )));

    let mut stream = client
        .stream(ModelRequest {
            conversation,
            tools: Vec::new(),
        })
        .await?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ModelEvent::ReasoningDelta(_) => {}
            ModelEvent::TextDelta(text) => output.push_str(&text),
            ModelEvent::Completed => {
                let output = output.trim().to_string();
                if output.is_empty() {
                    return Err(RuntimeError::AgentRun(
                        "summary model returned an empty summary".to_string(),
                    ));
                }
                return Ok(output);
            }
            ModelEvent::ToolCalls(_) => {
                return Err(RuntimeError::AgentRun(
                    "summary model requested tool calls".to_string(),
                ));
            }
        }
    }

    Err(RuntimeError::AgentRun(
        "summary model stream ended before completion".to_string(),
    ))
}

fn build_summary_prompt(
    existing_summary: Option<&str>,
    target_tokens: usize,
    repair_feedback: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(
        prompt,
        "Update the session summary. Target length: at most {target_tokens} tokens."
    );
    let _ = writeln!(
        prompt,
        "Output exactly one <analysis> block followed by one <summary> block."
    );
    let _ = writeln!(
        prompt,
        "The <summary> block must contain these section headings exactly:"
    );
    for section in REQUIRED_SUMMARY_SECTIONS {
        let _ = writeln!(prompt, "- {section}");
    }
    let _ = writeln!(prompt);
    let _ = writeln!(
        prompt,
        "Preserve user goals, constraints, decisions, file paths, code state, commands, results, errors, pending tasks, and open questions. Do not continue the conversation."
    );
    if let Some(feedback) = repair_feedback.filter(|feedback| !feedback.trim().is_empty()) {
        let _ = writeln!(prompt);
        let _ = writeln!(
            prompt,
            "Repair feedback from the previous invalid compact output:"
        );
        let _ = writeln!(prompt, "{feedback}");
    }
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Existing summary:");
    let _ = writeln!(prompt, "{}", existing_summary.unwrap_or("(none)"));
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Turns to incorporate:");

    for (offset, record) in records.iter().enumerate() {
        append_turn_record_transcript(&mut prompt, first_turn_index + offset, record);
    }

    prompt
}

fn append_turn_record_transcript(output: &mut String, index: usize, record: &TurnRecord) {
    let _ = writeln!(
        output,
        "\nTurn {index}: status={}",
        turn_status_label(record.turn.status)
    );
    if let Some(error) = record.turn.error.as_ref() {
        let _ = writeln!(output, "turn_error: {error}");
    }
    for message in &record.messages {
        let _ = writeln!(output, "{}:", message_role_label(message));
        if let Some(content) = message.content.as_ref() {
            let _ = writeln!(output, "{content}");
        }
        if let Some(tool_calls) = message.tool_calls.as_ref() {
            let tool_calls = serde_json::to_string(tool_calls).unwrap_or_else(|_| "[]".to_string());
            let _ = writeln!(output, "tool_calls: {tool_calls}");
        }
        if let Some(tool_call_id) = message.tool_call_id.as_ref() {
            let _ = writeln!(output, "tool_call_id: {tool_call_id}");
        }
    }
}

fn parse_compact_summary_output(output: &str) -> Result<String, String> {
    let normalized = strip_outer_markdown_code_fence(output);
    let summary = extract_xml_block(&normalized, "summary")?
        .ok_or_else(|| "compact response missing <summary> block".to_string())?;
    if summary.trim().is_empty() {
        return Err("compact summary response was empty".to_string());
    }
    if let Some(section) = REQUIRED_SUMMARY_SECTIONS
        .iter()
        .find(|section| !summary.contains(**section))
    {
        return Err(format!(
            "compact summary missing required section: {section}"
        ));
    }
    Ok(summary.trim().to_string())
}

fn extract_xml_block(content: &str, tag: &str) -> Result<Option<String>, String> {
    let Some((_open_start, open_end)) = find_opening_tag(content, tag) else {
        return Ok(None);
    };
    let Some((close_start, _close_end)) = find_closing_tag(&content[open_end..], tag) else {
        return Err(format!("compact response missing closing </{tag}> tag"));
    };
    let close_start = open_end + close_start;
    Ok(Some(content[open_end..close_start].trim().to_string()))
}

fn find_opening_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let mut start = 0;
    while let Some(relative) = lower[start..].find(&needle) {
        let tag_start = start + relative;
        let after = lower[tag_start + needle.len()..].chars().next();
        if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
            start = tag_start + needle.len();
            continue;
        }
        let tag_end = lower[tag_start..].find('>')? + tag_start + 1;
        return Some((tag_start, tag_end));
    }
    None
}

fn find_closing_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("</{tag}");
    let start = lower.find(&needle)?;
    let after = lower[start + needle.len()..].chars().next();
    if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
        return None;
    }
    let end = lower[start..].find('>')? + start + 1;
    Some((start, end))
}

fn strip_outer_markdown_code_fence(content: &str) -> String {
    let mut current = content.trim().to_string();
    loop {
        let stripped = strip_markdown_code_fence(&current);
        if stripped == current {
            return current;
        }
        current = stripped;
    }
}

fn strip_markdown_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let mut lines = trimmed.lines();
    let Some(first_line) = lines.next() else {
        return trimmed.to_string();
    };
    if !first_line.trim_start().starts_with("```") {
        return trimmed.to_string();
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim().to_string()
}

fn deterministic_session_summary(
    existing_summary: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> String {
    let mut summary = String::new();
    let _ = writeln!(summary, "User Goals and Constraints");
    let _ = writeln!(
        summary,
        "- Previous summary: {}",
        existing_summary
            .map(|summary| truncate_summary_text(summary, 1_200))
            .unwrap_or_else(|| "(none)".to_string())
    );
    let _ = writeln!(
        summary,
        "- Compacted {} turn records with deterministic fallback.",
        records.len()
    );
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Important Decisions");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Files and Code State");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Commands, Results, and Errors");
    append_fallback_errors(&mut summary, records, first_turn_index);
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Current Progress");
    for (offset, record) in records.iter().enumerate().rev().take(6).rev() {
        let index = first_turn_index + offset;
        let _ = writeln!(
            summary,
            "- Turn {index}: status={}",
            turn_status_label(record.turn.status)
        );
        if let Some(content) = record.turn.user_message.content.as_ref() {
            let _ = writeln!(summary, "  user: {}", truncate_summary_text(content, 240));
        }
        if let Some(message) = record.turn.assistant_message.as_ref()
            && let Some(content) = message.content.as_ref()
        {
            let _ = writeln!(
                summary,
                "  assistant: {}",
                truncate_summary_text(content, 240)
            );
        }
    }
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Pending Tasks");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Open Questions");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");

    summary.trim().to_string()
}

fn append_fallback_errors(output: &mut String, records: &[TurnRecord], first_turn_index: usize) {
    let mut wrote = false;
    for (offset, record) in records.iter().enumerate() {
        if let Some(error) = record.turn.error.as_ref() {
            let _ = writeln!(
                output,
                "- Turn {} error: {}",
                first_turn_index + offset,
                truncate_summary_text(error, 320)
            );
            wrote = true;
        }
    }
    if !wrote {
        let _ = writeln!(output, "- (none recorded)");
    }
}

fn truncate_summary_text(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.trim().to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn estimate_context_tokens(
    system_prompt: &str,
    session: &Session,
    prompt: &str,
    tools: &[ToolDefinition],
) -> usize {
    let tool_tokens = serde_json::to_string(tools)
        .map(|definitions| estimate_text_tokens(&definitions))
        .unwrap_or_default();
    let raw_total = message_text_tokens(agent_protocol::Role::System, system_prompt)
        + message_text_tokens(agent_protocol::Role::User, prompt)
        + tool_tokens
        + session
            .active_thread
            .messages
            .iter()
            .map(message_context_tokens)
            .sum::<usize>();
    raw_total
        .saturating_mul(REQUEST_PADDING_NUMERATOR)
        .div_ceil(REQUEST_PADDING_DENOMINATOR)
}

fn message_context_tokens(message: &Message) -> usize {
    let mut total = MESSAGE_BASE_TOKENS + estimate_text_tokens(message_role_label(message));
    if let Some(content) = message.content.as_ref() {
        total += estimate_text_tokens(content);
    }
    if let Some(reasoning_content) = message.reasoning_content.as_ref() {
        total += estimate_text_tokens(reasoning_content);
    }
    if let Some(tool_call_id) = message.tool_call_id.as_ref() {
        total += estimate_text_tokens(tool_call_id);
    }
    if let Some(tool_calls) = message.tool_calls.as_ref() {
        total += TOOL_CALL_BASE_TOKENS
            + serde_json::to_string(tool_calls)
                .map(|value| estimate_text_tokens(&value))
                .unwrap_or_default();
    }
    total
}

fn message_text_tokens(role: agent_protocol::Role, content: &str) -> usize {
    let mut total = MESSAGE_BASE_TOKENS + estimate_text_tokens(role_label(role));
    total += estimate_text_tokens(content);
    total
}

fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    let mut ascii_chars = 0usize;
    let mut non_ascii_tokens = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_tokens += 1;
        }
    }
    ascii_chars.div_ceil(4) + non_ascii_tokens
}

fn role_label(role: agent_protocol::Role) -> &'static str {
    match role {
        agent_protocol::Role::System => "system",
        agent_protocol::Role::User => "user",
        agent_protocol::Role::Assistant => "assistant",
        agent_protocol::Role::Tool => "tool",
    }
}

fn message_role_label(message: &Message) -> &'static str {
    match message.role {
        agent_protocol::Role::System => "system",
        agent_protocol::Role::User => "user",
        agent_protocol::Role::Assistant => "assistant",
        agent_protocol::Role::Tool => "tool",
    }
}

fn turn_status_label(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
    }
}

fn manifest_has_workspace_header(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| line.trim() == "[workspace]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::{OpenAiCompatClient, OpenAiCompatConfig};
    use agent_protocol::{FileChangeOperation, ReasoningLevel, SessionContext, Thread, Turn};
    use futures_util::future::BoxFuture;
    use serde_json::json;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_model_invocation() -> &'static ModelInvocation {
        static MODEL: OnceLock<ModelInvocation> = OnceLock::new();
        MODEL.get_or_init(|| ModelInvocation {
            provider_id: "test-provider".to_string(),
            provider_name: "Test Provider".to_string(),
            model_id: "test-model".to_string(),
            model_name: "Test Model".to_string(),
            reasoning: ReasoningLevel::Off,
        })
    }

    async fn spawn_recording_sse_server(
        bodies: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            for body in bodies {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 8192];
                let read = socket.read(&mut request).await.expect("read request");
                captured_requests
                    .lock()
                    .expect("requests lock poisoned")
                    .push(String::from_utf8_lossy(&request[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn client(base_url: String) -> OpenAiCompatClient {
        OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client")
    }

    fn sse_text_body(text: &str) -> &'static str {
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"content": text},
                    "finish_reason": null
                }]
            })
        );
        Box::leak(body.into_boxed_str())
    }

    fn tool_call_body(id: &str, name: &str, arguments: serde_json::Value) -> &'static str {
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string()
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            })
        );
        Box::leak(body.into_boxed_str())
    }

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-runtime-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create root");
        path
    }

    fn context_config(retain_recent_turns: usize) -> ContextConfig {
        ContextConfig {
            auto_compact: true,
            auto_compact_threshold: 0.835,
            retain_recent_turns,
            summary_target_tokens: 256,
            compact_max_retries: 2,
        }
    }

    fn model_limits(context_window_tokens: usize) -> ModelContextLimits {
        ModelContextLimits {
            context_window_tokens,
            reserved_output_tokens: 1,
        }
    }

    fn valid_compact_summary_text(current_progress: &str) -> String {
        format!(
            r#"User Goals and Constraints
- keep user intent

Important Decisions
- compact

Files and Code State
- none

Commands, Results, and Errors
- none

Current Progress
- {current_progress}

Pending Tasks
- none

Open Questions
- none"#
        )
    }

    fn valid_compact_summary(current_progress: &str) -> String {
        format!(
            r#"<analysis>
compact test
</analysis>
<summary>
{}
</summary>"#,
            valid_compact_summary_text(current_progress)
        )
    }

    fn completed_record(user: &str, assistant: &str) -> TurnRecord {
        let user_message = Message::user(user);
        let assistant_message = Message::assistant(assistant);
        let mut turn = Turn::running(user_message.clone());
        turn.complete(assistant_message.clone());
        TurnRecord::new(turn, vec![user_message, assistant_message])
    }

    fn compactable_session() -> Session {
        let turns = vec![
            completed_record("u0", "a0"),
            completed_record("u1", "a1"),
            TurnRecord::failed_user_prompt("broken", "failure reason"),
            completed_record("u3", "a3"),
            completed_record("u4", "a4"),
        ];
        let mut session = Session {
            active_thread: Thread::new(),
            turns,
            context: SessionContext::new(),
        };
        rebuild_active_thread(&mut session);
        session
    }

    #[derive(Default)]
    struct RecordingHandler {
        events: Vec<AgentEventEnvelope>,
    }

    impl TurnEventHandler for RecordingHandler {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            self.events.push(event.clone());
            Ok(())
        }
    }

    struct FailOnAgentMessage;

    impl TurnEventHandler for FailOnAgentMessage {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            if matches!(event.event, AgentEvent::AgentMessage(_)) {
                return Err(RuntimeError::event_handler("simulated output failure"));
            }
            Ok(())
        }
    }

    struct FailOnTextDelta;

    impl TurnEventHandler for FailOnTextDelta {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            if matches!(event.event, AgentEvent::TextDelta(_)) {
                return Err(RuntimeError::event_handler("simulated streaming failure"));
            }
            Ok(())
        }
    }

    struct PendingModel;

    impl Model for PendingModel {
        fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
            async move {
                let stream: agent_core::ModelStream = Box::pin(futures_util::stream::pending::<
                    Result<ModelEvent, ModelFailure>,
                >());
                Ok(stream)
            }
            .boxed()
        }
    }

    #[derive(Clone)]
    struct ConstantModel {
        text: String,
    }

    impl Model for ConstantModel {
        fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
            let text = self.text.clone();
            async move {
                let stream: agent_core::ModelStream = futures_util::stream::iter(vec![
                    Ok(ModelEvent::TextDelta(text)),
                    Ok(ModelEvent::Completed),
                ])
                .boxed();
                Ok(stream)
            }
            .boxed()
        }
    }

    #[derive(Clone)]
    struct GatedModel {
        started: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Barrier>,
    }

    impl Model for GatedModel {
        fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            async move {
                started.fetch_add(1, Ordering::AcqRel);
                release.wait().await;
                let stream: agent_core::ModelStream = futures_util::stream::iter(vec![
                    Ok(ModelEvent::TextDelta("done".to_string())),
                    Ok(ModelEvent::Completed),
                ])
                .boxed();
                Ok(stream)
            }
            .boxed()
        }
    }

    struct CancelOnTurnStarted {
        cancellation: CancellationToken,
        events: Vec<AgentEventEnvelope>,
    }

    impl TurnEventHandler for CancelOnTurnStarted {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            self.events.push(event.clone());
            if matches!(event.event, AgentEvent::TurnStarted) {
                self.cancellation.cancel();
            }
            Ok(())
        }
    }

    struct ApprovalHandler {
        events: Vec<AgentEventEnvelope>,
        approved: bool,
    }

    impl TurnEventHandler for ApprovalHandler {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            self.events.push(event.clone());
            Ok(())
        }

        fn resolve_approval<'a>(
            &'a mut self,
            request: &'a ApprovalRequest,
        ) -> BoxFuture<'a, Result<ApprovalDecision, RuntimeError>> {
            async move {
                Ok(if self.approved {
                    ApprovalDecision::approve(request.id.clone())
                } else {
                    ApprovalDecision::deny(request.id.clone())
                })
            }
            .boxed()
        }
    }

    #[test]
    fn event_envelope_uses_stable_schema_and_indices() {
        let root = unique_dir("envelope");
        let envelope = make_event_envelope("default", &root, 7, 3, AgentEvent::TurnStarted);

        assert_eq!(envelope.schema_version, EVENT_SCHEMA_VERSION);
        assert!(envelope.timestamp_ms > 0);
        assert_eq!(envelope.session, "default");
        assert_eq!(envelope.workspace_root, root.display().to_string());
        assert_eq!(envelope.turn_index, 7);
        assert_eq!(envelope.event_index, 3);
        assert_eq!(envelope.event, AgentEvent::TurnStarted);
    }

    #[tokio::test]
    async fn subagent_executor_runs_four_tasks_concurrently_and_rejects_the_fifth() {
        let root = unique_dir("subagent-limit");
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Barrier::new(MAX_SUBAGENTS_PER_TURN + 1));
        let executor = RuntimeSubagentExecutor::new(
            Arc::new(GatedModel {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
            Arc::<str>::from("system"),
            Arc::new(root),
        );
        let cancellation = CancellationToken::new();
        let futures = (0..MAX_SUBAGENTS_PER_TURN)
            .map(|index| executor.execute(format!("task {index}"), cancellation.clone()))
            .collect::<Vec<_>>();
        let join = tokio::spawn(async move { futures_util::future::join_all(futures).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::Acquire) < MAX_SUBAGENTS_PER_TURN {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all subagents should start concurrently");
        release.wait().await;
        let summaries = join.await.expect("join subagents");
        assert!(summaries.iter().all(|summary| summary.error.is_none()));

        let rejected = executor
            .execute("fifth task".to_string(), cancellation)
            .await;
        assert_eq!(
            rejected.error.as_deref(),
            Some("subagent limit exceeded (4 per turn)")
        );
        assert_eq!(started.load(Ordering::Acquire), MAX_SUBAGENTS_PER_TURN);
    }

    #[tokio::test]
    async fn subagent_timeout_does_not_cancel_the_parent_token() {
        let root = unique_dir("subagent-timeout");
        let mut executor = RuntimeSubagentExecutor::new(
            Arc::new(PendingModel),
            Arc::<str>::from("system"),
            Arc::new(root),
        );
        executor.timeout = Duration::from_millis(10);
        let parent_cancellation = CancellationToken::new();

        let summary = executor
            .execute("wait forever".to_string(), parent_cancellation.clone())
            .await;

        assert!(
            summary
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out"))
        );
        assert!(!parent_cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn parent_cancellation_stops_a_running_subagent() {
        let root = unique_dir("subagent-cancel");
        let executor = RuntimeSubagentExecutor::new(
            Arc::new(PendingModel),
            Arc::<str>::from("system"),
            Arc::new(root),
        );
        let parent_cancellation = CancellationToken::new();
        let run = executor.execute("wait forever".to_string(), parent_cancellation.clone());
        let worker = tokio::spawn(run);

        tokio::task::yield_now().await;
        parent_cancellation.cancel();
        let summary = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("cancelled subagent should stop")
            .expect("subagent worker");

        assert_eq!(
            summary.error.as_deref(),
            Some("subagent execution cancelled")
        );
    }

    #[tokio::test]
    async fn subagent_results_are_truncated_on_unicode_boundaries() {
        let root = unique_dir("subagent-truncate");
        let mut executor = RuntimeSubagentExecutor::new(
            Arc::new(ConstantModel {
                text: "甲乙丙丁".to_string(),
            }),
            Arc::<str>::from("system"),
            Arc::new(root),
        );
        executor.max_result_chars = 3;

        let summary = executor
            .execute("unicode result".to_string(), CancellationToken::new())
            .await;

        assert_eq!(summary.result.as_deref(), Some("甲乙丙"));
        assert!(summary.truncated);
        assert_eq!(summary.model_calls, 1);
        assert_eq!(summary.tool_calls, 0);
    }

    #[test]
    fn context_estimate_includes_tool_definitions() {
        let session = Session::new();
        let without_tools = estimate_context_tokens("system", &session, "hello", &[]);
        let tools = vec![ToolDefinition::function(
            "large_tool",
            "x".repeat(4_000),
            json!({"type": "object", "properties": {}}),
        )];

        let with_tools = estimate_context_tokens("system", &session, "hello", &tools);

        assert!(with_tools > without_tools + 1_000);
    }

    #[test]
    fn context_estimate_includes_reasoning_content() {
        let mut without_reasoning = Session::new();
        without_reasoning
            .active_thread
            .push(Message::assistant("answer"));
        let mut with_reasoning = without_reasoning.clone();
        with_reasoning.active_thread.messages[0].reasoning_content = Some("r".repeat(4_000));

        let without = estimate_context_tokens("system", &without_reasoning, "hello", &[]);
        let with = estimate_context_tokens("system", &with_reasoning, "hello", &[]);

        assert!(with > without + 1_000);
    }

    #[test]
    fn summary_prompt_omits_reasoning_content() {
        let user = Message::user("question");
        let assistant =
            Message::assistant("answer").with_reasoning_content("private reasoning chain");
        let mut turn = Turn::running(user.clone());
        turn.complete(assistant.clone());
        let record = TurnRecord::new(turn, vec![user, assistant]);

        let prompt = build_summary_prompt(None, 256, None, &[record], 0);

        assert!(prompt.contains("answer"));
        assert!(!prompt.contains("private reasoning chain"));
    }

    #[tokio::test]
    async fn manual_compaction_summarizes_old_turns_and_rebuilds_active_context() {
        let summary = valid_compact_summary("new summary");
        let summary_text = valid_compact_summary_text("new summary");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body(&summary)]).await;
        let mut session = compactable_session();

        let outcome = compact_session(&client(base_url), &mut session, context_config(2))
            .await
            .expect("compact session");

        assert_eq!(outcome, CompactionOutcome::Changed);
        assert_eq!(
            session.context.summary.as_deref(),
            Some(summary_text.as_str())
        );
        assert_eq!(session.context.summarized_turns, 3);
        assert_eq!(
            session.active_thread.messages,
            vec![
                Message::system(format!("Session summary:\n{summary_text}")),
                Message::user("u3"),
                Message::assistant("a3"),
                Message::user("u4"),
                Message::assistant("a4"),
            ]
        );

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("failure reason"));
        assert!(requests[0].contains("Target length: at most 256 tokens"));
    }

    #[test]
    fn compact_summary_parser_accepts_markdown_fenced_contract() {
        let summary_text = valid_compact_summary_text("fenced summary");
        let raw = format!(
            "```xml\n<analysis>\nprivate\n</analysis>\n<summary>\n{summary_text}\n</summary>\n```"
        );

        let parsed = parse_compact_summary_output(&raw).expect("parse summary");

        assert_eq!(parsed, summary_text);
    }

    #[tokio::test]
    async fn compaction_retries_invalid_contract_with_repair_feedback() {
        let valid_summary = valid_compact_summary("retry summary");
        let valid_summary_text = valid_compact_summary_text("retry summary");
        let (base_url, requests) = spawn_recording_sse_server(vec![
            sse_text_body("<analysis>bad</analysis><summary>too short</summary>"),
            sse_text_body(&valid_summary),
        ])
        .await;
        let mut session = compactable_session();

        let outcome = compact_session(&client(base_url), &mut session, context_config(2))
            .await
            .expect("compact session");

        assert_eq!(outcome, CompactionOutcome::Changed);
        assert_eq!(
            session.context.summary.as_deref(),
            Some(valid_summary_text.as_str())
        );
        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("Repair feedback"));
        assert!(requests[1].contains("missing required section"));
    }

    #[tokio::test]
    async fn run_agent_turn_records_completed_turn_and_event_envelopes() {
        let root = unique_dir("run-success");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(
            outcome,
            RunAgentTurnOutcome {
                session_changed: true,
                error: None,
            }
        );
        assert_eq!(
            session.active_thread.messages,
            vec![Message::user("hello"), Message::assistant("ok")]
        );
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
        assert_eq!(
            session.turns[0].turn.model.as_ref(),
            Some(test_model_invocation())
        );
        assert_eq!(session.turns[0].messages, session.active_thread.messages);
        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
        assert_eq!(
            handler
                .events
                .iter()
                .map(|event| event.event_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            handler.events[1].event,
            AgentEvent::TextDelta("ok".to_string())
        );
    }

    #[tokio::test]
    async fn delegate_task_runs_an_isolated_read_only_subagent() {
        let root = unique_dir("subagent-success");
        fs::write(root.join("note.txt"), "workspace evidence\n").expect("write note");
        let (base_url, requests) = spawn_recording_sse_server(vec![
            tool_call_body(
                "delegate-1",
                "delegate_task",
                json!({"task": "Read note.txt and report the evidence"}),
            ),
            tool_call_body(
                "read-1",
                "read_file",
                json!({"path": "note.txt", "max_lines": 20}),
            ),
            sse_text_body("The file contains workspace evidence."),
            sse_text_body("The subagent confirmed the workspace evidence."),
        ])
        .await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "project policy",
                context_config: context_config(2),
                model_limits: model_limits(100_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(
                    agent_protocol::PermissionMode::DangerFullAccess,
                ),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "Use a subagent to inspect the note",
            &mut handler,
        )
        .await
        .expect("run delegated turn");

        assert_eq!(outcome.error, None);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
        assert_eq!(session.turns[0].messages.len(), 4);
        assert!(session.turns[0].messages.iter().any(|message| {
            message.role == agent_protocol::Role::Tool
                && message.content.as_deref().is_some_and(|content| {
                    content.contains("The file contains workspace evidence.")
                })
        }));
        assert!(!session.turns[0].messages.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content == "workspace evidence\n")
        }));
        assert!(handler.events.iter().any(|event| matches!(
            &event.event,
            AgentEvent::SubagentStarted {
                id,
                agent_name: Some(agent_name),
                task,
            }
                if id == "delegate-1"
                    && !agent_name.is_empty()
                    && task == "Read note.txt and report the evidence"
        )));
        let started_name = handler
            .events
            .iter()
            .find_map(|event| match &event.event {
                AgentEvent::SubagentStarted { agent_name, .. } => agent_name.as_deref(),
                _ => None,
            })
            .expect("subagent start name");
        assert!(handler.events.iter().any(|event| matches!(
            &event.event,
            AgentEvent::SubagentFinished { id, ok: true, summary }
                if id == "delegate-1"
                    && summary.agent_name.as_deref() == Some(started_name)
                    && summary.model_calls == 2
                    && summary.tool_calls == 1
                    && summary.result.as_deref() == Some("The file contains workspace evidence.")
        )));

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("delegate_task"));
        assert!(requests[0].contains(PARENT_SUBAGENT_GUIDANCE));
        assert!(requests[1].contains("Read note.txt and report the evidence"));
        assert!(requests[1].contains(CHILD_SUBAGENT_GUIDANCE));
        assert!(requests[1].contains("read_file"));
        assert!(requests[1].contains("list_files"));
        assert!(requests[1].contains("search_text"));
        assert!(!requests[1].contains("delegate_task"));
        assert!(!requests[1].contains("write_file"));
        assert!(!requests[1].contains("shell_command"));
        assert!(!requests[1].contains("Use a subagent to inspect the note"));
        assert!(requests[2].contains("workspace evidence"));
        assert!(requests[3].contains("The file contains workspace evidence."));
    }

    #[tokio::test]
    async fn event_handler_failure_after_completion_commits_turn_and_reports_error() {
        let root = unique_dir("handler-failure");
        let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::from_thread(Thread {
            messages: vec![Message::user("before"), Message::assistant("context")],
        });
        let mut handler = FailOnAgentMessage;
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("handler failure is reported after committing the terminal turn");

        assert!(outcome.session_changed);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|error| error.contains("simulated output failure"))
        );
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
        assert_eq!(
            session.active_thread.messages,
            vec![
                Message::user("before"),
                Message::assistant("context"),
                Message::user("hello"),
                Message::assistant("ok"),
            ]
        );
    }

    #[tokio::test]
    async fn event_handler_failure_mid_turn_records_failed_turn() {
        let root = unique_dir("handler-streaming-failure");
        let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let original_thread = Thread {
            messages: vec![Message::user("before"), Message::assistant("context")],
        };
        let mut session = Session::from_thread(original_thread.clone());
        let mut handler = FailOnTextDelta;
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("handler failure must still produce an auditable outcome");

        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|error| error.contains("simulated streaming failure"))
        );
        assert_eq!(session.active_thread, original_thread);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Failed);
        assert_eq!(
            session.turns[0].turn.model.as_ref(),
            Some(test_model_invocation())
        );
        assert!(
            session.turns[0]
                .turn
                .error
                .as_deref()
                .is_some_and(|error| error.contains("simulated streaming failure"))
        );
    }

    #[tokio::test]
    async fn cancellation_records_failed_turn_without_changing_active_context() {
        let root = unique_dir("cancelled-turn");
        let model = PendingModel;
        let original_thread = Thread {
            messages: vec![Message::user("before"), Message::assistant("context")],
        };
        let mut session = Session::from_thread(original_thread.clone());
        let cancellation = CancellationToken::new();
        let mut handler = CancelOnTurnStarted {
            cancellation: cancellation.clone(),
            events: Vec::new(),
        };
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn_with_cancellation(
            RunAgentTurnContext {
                client: &model,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(1_000_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "cancel me",
            &mut handler,
            cancellation,
        )
        .await
        .expect("cancelled turn should close normally");

        assert_eq!(
            outcome,
            RunAgentTurnOutcome {
                session_changed: true,
                error: Some("turn cancelled".to_string()),
            }
        );
        assert_eq!(session.active_thread, original_thread);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Failed);
        assert_eq!(
            session.turns[0].turn.model.as_ref(),
            Some(test_model_invocation())
        );
        assert_eq!(
            session.turns[0].turn.error.as_deref(),
            Some("turn cancelled")
        );
        assert_eq!(session.turns[0].messages, vec![Message::user("cancel me")]);
        assert!(
            handler
                .events
                .iter()
                .any(|event| event.event == AgentEvent::Error("turn cancelled".to_string()))
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn inspect_mcp_servers_returns_discovered_tools() {
        let root = unique_dir("inspect-mcp-tools");
        let server_script = root.join("fake-inspection-mcp.sh");
        fs::write(
            &server_script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"Search Docs","description":"Search docs","inputSchema":{"type":"object"}}]}}'
  fi
done
"#,
        )
        .expect("write fake MCP server");
        let server = McpServerConfig {
            name: "Docs".to_string(),
            transport: agent_config::McpTransport::Stdio,
            command: "sh".to_string(),
            args: vec![server_script.display().to_string()],
            env: Default::default(),
            cwd: None,
            url: None,
            http_headers: Default::default(),
            enabled: true,
            startup_timeout_sec: 5,
            tool_timeout_sec: 5,
        };

        let inspection = inspect_mcp_servers(&root, &[server]).await;

        assert!(inspection.diagnostics.is_empty());
        assert_eq!(inspection.tools.len(), 1);
        assert_eq!(inspection.tools[0].name, "mcp__docs__search_docs");
        assert!(inspection.tools[0].description.contains("Search docs"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn run_agent_turn_includes_mcp_tool_definitions_in_model_request() {
        let root = unique_dir("run-mcp-tools");
        let server_script = root.join("fake-mcp.sh");
        fs::write(
            &server_script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"Search Docs","description":"Search docs","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}]}}'
  fi
done
"#,
        )
        .expect("write fake MCP server");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();
        let mcp_servers = vec![McpServerConfig {
            name: "Docs".to_string(),
            transport: agent_config::McpTransport::Stdio,
            command: "sh".to_string(),
            args: vec![server_script.display().to_string()],
            env: Default::default(),
            cwd: None,
            url: None,
            http_headers: Default::default(),
            enabled: true,
            startup_timeout_sec: 5,
            tool_timeout_sec: 5,
        }];

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &mcp_servers,
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("mcp__docs__search_docs"));
        assert!(requests[0].contains("Search docs"));
    }

    #[tokio::test]
    async fn run_agent_turn_emits_mcp_diagnostics_as_warnings() {
        let root = unique_dir("run-mcp-warning");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();
        let mcp_servers = vec![McpServerConfig {
            name: "bad".to_string(),
            transport: agent_config::McpTransport::Stdio,
            command: "definitely-not-a-real-morrow-mcp-command".to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            url: None,
            http_headers: Default::default(),
            enabled: true,
            startup_timeout_sec: 1,
            tool_timeout_sec: 1,
        }];

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &mcp_servers,
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
        assert!(matches!(
            &handler.events[0].event,
            AgentEvent::Warning(message)
                if message.contains("mcp server bad")
                    && message.contains("failed to start MCP stdio server")
        ));
        assert_eq!(handler.events[0].event_index, 0);
        assert_eq!(handler.events[1].event, AgentEvent::TurnStarted);
        assert_eq!(handler.events[1].event_index, 1);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn run_agent_turn_reuses_mcp_cache_across_turns() {
        let root = unique_dir("run-mcp-cache");
        let server_script = root.join("fake-mcp.sh");
        let marker = root.join("started.txt");
        fs::write(
            &server_script,
            format!(
                r#"#!/bin/sh
printf 'started\n' >> '{}'
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"fake","version":"1"}}}}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"Search Docs","description":"Search docs","inputSchema":{{"type":"object"}}}}]}}}}'
  fi
done
"#,
                marker.display()
            ),
        )
        .expect("write fake MCP server");
        let (base_url, requests) =
            spawn_recording_sse_server(vec![sse_text_body("one"), sse_text_body("two")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut first_handler = RecordingHandler::default();
        let mut second_handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();
        let mcp_servers = vec![McpServerConfig {
            name: "Docs".to_string(),
            transport: agent_config::McpTransport::Stdio,
            command: "sh".to_string(),
            args: vec![server_script.display().to_string()],
            env: Default::default(),
            cwd: None,
            url: None,
            http_headers: Default::default(),
            enabled: true,
            startup_timeout_sec: 5,
            tool_timeout_sec: 5,
        }];

        for (turn_index, prompt, handler) in [
            (0, "hello", &mut first_handler),
            (1, "again", &mut second_handler),
        ] {
            let outcome = run_agent_turn(
                RunAgentTurnContext {
                    client: &client,
                    model: test_model_invocation(),
                    system_prompt: "system",
                    context_config: context_config(2),
                    model_limits: model_limits(10_000),
                    workspace_root: &root,
                    permissions: PermissionProfile::for_mode(
                        agent_protocol::PermissionMode::ReadOnly,
                    ),
                    mcp_servers: &mcp_servers,
                    mcp_cache: &mcp_cache,
                    session_name: "default",
                    turn_index,
                },
                &mut session,
                prompt,
                handler,
            )
            .await
            .expect("run turn");
            assert_eq!(outcome.error, None);
        }

        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 2);
        assert_eq!(
            fs::read_to_string(marker).expect("marker").lines().count(),
            1
        );
    }

    #[tokio::test]
    async fn approval_deny_path_resumes_stream_and_records_turn() {
        let root = unique_dir("approval-deny");
        let first_body = tool_call_body(
            "call_1",
            "write_file",
            json!({
                "path": "note.txt",
                "content": "created\n"
            }),
        );
        let second_body = sse_text_body("Denied");
        let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = ApprovalHandler {
            events: Vec::new(),
            approved: false,
        };
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(
                    agent_protocol::PermissionMode::WorkspaceWrite,
                ),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "write note",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
        assert!(
            handler
                .events
                .iter()
                .any(|event| matches!(event.event, AgentEvent::ApprovalRequested(_)))
        );
        assert!(handler.events.iter().any(|event| {
            matches!(
                &event.event,
                AgentEvent::ApprovalResolved(decision) if !decision.approved
            )
        }));
    }

    #[tokio::test]
    async fn auto_compaction_llm_failure_falls_back_and_runs_main_turn() {
        let root = unique_dir("run-compact-fallback");
        let (base_url, requests) =
            spawn_recording_sse_server(vec!["data: {not-json}\n\n", sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = compactable_session();
        session.turns[0] = completed_record(&"older user context ".repeat(1_000), "a0");
        rebuild_active_thread(&mut session);
        let mut handler = RecordingHandler::default();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(2_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                session_name: "default",
                turn_index: session.turns.len(),
            },
            &mut session,
            "hello",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(
            outcome,
            RunAgentTurnOutcome {
                session_changed: true,
                error: None,
            }
        );
        assert_eq!(session.turns.len(), 6);
        assert_eq!(
            session.turns.last().expect("failed turn").turn.status,
            TurnStatus::Completed
        );
        assert!(
            session
                .context
                .summary
                .as_deref()
                .expect("fallback summary")
                .contains("deterministic fallback")
        );
        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 2);
        assert!(!handler.events.is_empty());
    }

    #[test]
    fn file_summary_helper_is_available_to_tests() {
        let file = agent_protocol::FileChangeSummary {
            path: "note.txt".to_string(),
            operation: FileChangeOperation::Add,
            replacements: 0,
            created: true,
            overwritten: false,
            deleted: false,
        };

        assert_eq!(file.operation.as_str(), "add");
    }
}
