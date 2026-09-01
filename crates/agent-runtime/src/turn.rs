use super::*;

pub const EVENT_SCHEMA_VERSION: u32 = 8;
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
    #[error(transparent)]
    SubagentStore(#[from] SubagentStoreError),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    pub schema_version: u32,
    pub timestamp_ms: u64,
    pub session: String,
    pub workspace_root: String,
    #[serde(default)]
    pub origin: AgentEventOrigin,
    pub turn_index: usize,
    pub event_index: usize,
    pub event: AgentEvent,
}

#[derive(Clone, Copy)]
pub struct RunAgentTurnContext<'a> {
    pub client: &'a dyn Model,
    pub model: &'a ModelInvocation,
    pub subagent_identities: &'a [SubagentIdentity],
    /// 配置层 base prompt（不含 AGENTS.md）；AGENTS.md 段落每轮经
    /// `workspace_instructions` 缓存重读后再拼接。
    pub system_prompt: &'a str,
    pub context_config: ContextConfig,
    pub model_limits: ModelContextLimits,
    pub workspace_root: &'a Path,
    /// 每轮重读 AGENTS.md 的进程级缓存；`None` 时 `system_prompt` 原样使用。
    pub workspace_instructions: Option<&'a WorkspaceInstructionsCache>,
    pub permissions: PermissionProfile,
    pub mcp_servers: &'a [McpServerConfig],
    pub mcp_cache: &'a McpToolCache,
    /// `[tools] allow/deny` 过滤；`None` 等价于全量允许。
    pub tools: Option<&'a ToolsConfig>,
    /// workspace_write 模式下 workspace 内文件变更是否自动放行（false = 逐次审批旧行为）。
    pub auto_approve_workspace_writes: bool,
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

struct AgentTurnExecution<'a> {
    context: RunAgentTurnContext<'a>,
    prompt: &'a str,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
    middleware: MiddlewareRunConfig<'a>,
    /// prepared session 路径在写 `TurnStarted` fact 时已拼好的 turn base
    /// （含 AGENTS.md 与 `<environment>` 块），透传以保证 fact 与模型请求一致。
    prepared_system_prompt: Option<String>,
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

pub(crate) struct SessionFactRun<'a> {
    handle: &'a SessionHandle,
    operation_id: String,
    turn_id: String,
    current_model_call_id: String,
    next_model_call_index: usize,
    tool_calls: HashMap<String, ToolCall>,
}

enum OperationUpdate {
    None,
    Phase(&'static str),
    ClearStreaming(&'static str),
}

impl<'a> SessionFactRun<'a> {
    pub(crate) fn new(handle: &'a SessionHandle, operation_id: String, turn_id: String) -> Self {
        Self {
            handle,
            operation_id,
            turn_id,
            current_model_call_id: "model-call-0".to_string(),
            next_model_call_index: 0,
            tool_calls: HashMap::new(),
        }
    }

    pub(crate) async fn persist_event(&mut self, event: &AgentEvent) -> Result<(), RuntimeError> {
        let (fact, operation_update) = match event {
            AgentEvent::TurnStarted => (None, OperationUpdate::None),
            AgentEvent::MiddlewareStarted(_) => (None, OperationUpdate::None),
            AgentEvent::MiddlewareFinished(invocation) => (
                Some(SessionFact::MiddlewareFinished {
                    invocation: invocation.clone(),
                }),
                OperationUpdate::None,
            ),
            AgentEvent::ModelCallStarted => {
                let model_call_id = format!("model-call-{}", self.next_model_call_index);
                self.next_model_call_index += 1;
                self.current_model_call_id = model_call_id.clone();
                (
                    Some(SessionFact::ModelCallStarted { model_call_id }),
                    OperationUpdate::Phase("model_call"),
                )
            }
            AgentEvent::Warning(message) => (
                Some(SessionFact::NoticeRecorded {
                    message: message.clone(),
                }),
                OperationUpdate::None,
            ),
            AgentEvent::ReasoningDelta(delta) => {
                self.handle
                    .append_stream_delta(
                        &self.operation_id,
                        &self.current_model_call_id,
                        None,
                        Some(delta.clone()),
                    )
                    .await;
                (None, OperationUpdate::None)
            }
            AgentEvent::TextDelta(delta) => {
                self.handle
                    .append_stream_delta(
                        &self.operation_id,
                        &self.current_model_call_id,
                        Some(delta.clone()),
                        None,
                    )
                    .await;
                (None, OperationUpdate::None)
            }
            AgentEvent::ModelMessageCommitted {
                model_call_id,
                message,
            } => {
                for call in message.tool_calls.iter().flatten() {
                    self.tool_calls.insert(call.id.clone(), call.clone());
                }
                (
                    Some(SessionFact::ModelMessageCommitted {
                        model_call_id: model_call_id.clone(),
                        message: message.clone(),
                    }),
                    OperationUpdate::ClearStreaming("model_message_committed"),
                )
            }
            AgentEvent::ToolCallStarted { id, name } => (
                Some(SessionFact::ToolCallStarted {
                    tool_call: self
                        .tool_calls
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| ToolCall::function(id.clone(), name.clone(), "{}")),
                }),
                OperationUpdate::Phase("tool_call"),
            ),
            AgentEvent::SubagentStarted { id, task, .. } => (
                Some(SessionFact::ToolCallStarted {
                    tool_call: self.tool_calls.get(id).cloned().unwrap_or_else(|| {
                        ToolCall::function(
                            id.clone(),
                            "delegate_task",
                            serde_json::json!({"task": task}).to_string(),
                        )
                    }),
                }),
                OperationUpdate::Phase("subagent"),
            ),
            AgentEvent::ApprovalRequested(request) => (
                Some(SessionFact::ApprovalRequested {
                    request: request.clone(),
                }),
                OperationUpdate::Phase("approval"),
            ),
            AgentEvent::ApprovalResolved(decision) => (
                Some(SessionFact::ApprovalResolved {
                    decision: decision.clone(),
                }),
                OperationUpdate::Phase("tool_call"),
            ),
            AgentEvent::ToolResultCommitted {
                tool_call_id,
                message,
                ok,
                summary,
            } => (
                Some(SessionFact::ToolCallFinished {
                    tool_call_id: tool_call_id.clone(),
                    result: message.clone(),
                    ok: *ok,
                    summary: summary.clone(),
                }),
                OperationUpdate::None,
            ),
            AgentEvent::TurnCompleted => (Some(SessionFact::TurnCompleted), OperationUpdate::None),
            AgentEvent::Error(_) => (None, OperationUpdate::None),
            AgentEvent::AgentMessage(_)
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::SubagentUpdated(_)
            | AgentEvent::ToolCallFinished { .. } => (None, OperationUpdate::None),
        };
        if let Some(fact) = fact {
            self.handle
                .commit_fact(
                    Some(self.operation_id.clone()),
                    Some(self.turn_id.clone()),
                    fact,
                )
                .await?;
        }
        match operation_update {
            OperationUpdate::None => {}
            OperationUpdate::Phase(phase) => self.handle.set_operation_phase(phase).await,
            OperationUpdate::ClearStreaming(phase) => self.handle.clear_streaming(phase).await,
        }
        Ok(())
    }

    pub(crate) async fn persist_compaction(&self, session: &Session) -> Result<(), RuntimeError> {
        let Some(summary) = session.context.summary.as_ref() else {
            return Ok(());
        };
        if session.context.summarized_turns == 0 {
            return Ok(());
        }
        let projection = self.handle.projection().await;
        let Some(covered) = projection
            .turns
            .get(session.context.summarized_turns - 1)
            .map(|turn| turn.id.clone())
        else {
            return Ok(());
        };
        self.handle
            .commit_fact(
                Some(self.operation_id.clone()),
                None,
                SessionFact::ContextCompacted {
                    summary: summary.clone(),
                    covered_through_turn_id: covered,
                },
            )
            .await?;
        Ok(())
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

pub async fn run_agent_turn_with_middleware(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    middleware: &MiddlewareRegistry,
    agent_scope: MiddlewareAgentScope,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    run_agent_turn_with_middleware_context(
        MiddlewareAgentTurnContext::new(context, middleware, agent_scope),
        session,
        prompt,
        handler,
        cancellation,
    )
    .await
}

pub async fn run_agent_turn_with_middleware_context(
    context: MiddlewareAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let execution_context = context.execution_context(&cancellation, None, None);
    let before = context
        .registry
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: execution_context,
            prompt: prompt.to_string(),
        })
        .await;
    let before_cancelled = before.cancelled;
    let before_denied = before.denied();
    let denied_reasons = before.denied_reasons.clone();
    let event_index =
        deliver_middleware_events(context.turn, handler, None, before.events, 0).await?;
    if before_cancelled {
        return Ok(RunAgentTurnOutcome {
            session_changed: false,
            error: Some("operation cancelled".to_string()),
        });
    }
    if before_denied {
        return Ok(RunAgentTurnOutcome {
            session_changed: false,
            error: Some(format!(
                "prompt blocked by middleware: {}",
                denied_reasons.join("; ")
            )),
        });
    }
    run_agent_turn_with_optional_controller_and_facts(
        session,
        handler,
        AgentTurnExecution {
            context: context.turn,
            prompt,
            cancellation,
            controller: None,
            middleware: context.run_config(before.context, event_index),
            prepared_system_prompt: None,
        },
        None,
    )
    .await
}

pub async fn run_agent_turn_with_session_handle(
    context: RunAgentTurnContext<'_>,
    handle: &SessionHandle,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let middleware = MiddlewareRegistry::default();
    run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(context, &middleware, MiddlewareAgentScope::Main),
        handle,
        prompt,
        handler,
        cancellation,
        controller,
    )
    .await
}

// Compatibility shim for callers that still pass middleware fields separately.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_turn_with_session_handle_and_middleware(
    context: RunAgentTurnContext<'_>,
    handle: &SessionHandle,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
    middleware: &MiddlewareRegistry,
    agent_scope: MiddlewareAgentScope,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(context, middleware, agent_scope),
        handle,
        prompt,
        handler,
        cancellation,
        controller,
    )
    .await
}

pub async fn run_agent_turn_with_session_handle_and_middleware_context(
    context: MiddlewareAgentTurnContext<'_>,
    handle: &SessionHandle,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let persistent_controller = controller.is_some();
    let prepared = prepare_session_turn_with_middleware_context(
        context,
        handle,
        prompt,
        handler,
        &cancellation,
        persistent_controller,
    )
    .await?;
    let Some(prepared) = prepared else {
        return Ok(RunAgentTurnOutcome {
            session_changed: false,
            error: Some("prompt blocked by middleware".to_string()),
        });
    };
    run_agent_turn_with_prepared_session_handle_and_middleware_context(
        context,
        handle,
        prepared.with_prompt(prompt),
        handler,
        cancellation,
        controller,
    )
    .await
}

pub async fn prepare_session_turn_with_middleware(
    context: RunAgentTurnContext<'_>,
    handle: &SessionHandle,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: &CancellationToken,
    middleware: &MiddlewareRegistry,
    agent_scope: MiddlewareAgentScope,
) -> Result<Option<PreparedMiddlewareSessionTurn>, RuntimeError> {
    prepare_session_turn_with_middleware_context(
        MiddlewareAgentTurnContext::new(context, middleware, agent_scope),
        handle,
        prompt,
        handler,
        cancellation,
        false,
    )
    .await
}

pub async fn prepare_session_turn_with_middleware_context(
    context: MiddlewareAgentTurnContext<'_>,
    handle: &SessionHandle,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: &CancellationToken,
    persistent_controller: bool,
) -> Result<Option<PreparedMiddlewareSessionTurn>, RuntimeError> {
    let execution_context = context.execution_context(cancellation, None, None);
    let before = context
        .registry
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: execution_context,
            prompt: prompt.to_string(),
        })
        .await;
    let before_cancelled = before.cancelled;
    let before_denied = before.denied();
    let denied_reasons = before.denied_reasons.clone();
    let event_index =
        deliver_middleware_events(context.turn, handler, Some(handle), before.events, 0).await?;
    if before_cancelled {
        return Ok(None);
    }
    if before_denied {
        handle
            .commit_fact(
                None,
                None,
                SessionFact::PromptRejected {
                    prompt: prompt.to_string(),
                    reasons: denied_reasons.clone(),
                },
            )
            .await?;
        handle
            .notice(format!(
                "prompt blocked by middleware: {}",
                denied_reasons.join("; ")
            ))
            .await;
        return Ok(None);
    }
    let turn_base = assembled_turn_system_prompt(context.turn).await;
    let system_prompt = effective_turn_system_prompt(
        &turn_base,
        context.turn.client.shared_clone().is_some(),
        persistent_controller,
    );
    let (operation_id, turn_id) = handle
        .begin_operation(
            Message::user(prompt),
            context.turn.model.clone(),
            context.turn.permissions,
            system_prompt,
        )
        .await?;
    Ok(Some(PreparedMiddlewareSessionTurn {
        operation_id,
        turn_id,
        system_prompt: turn_base,
        initial_context: before.context,
        event_index,
    }))
}

pub struct PreparedSessionTurn<'a> {
    pub operation_id: String,
    pub turn_id: String,
    pub prompt: &'a str,
    /// 见 `PreparedMiddlewareSessionTurn::system_prompt`。
    pub system_prompt: String,
}

pub async fn run_agent_turn_with_prepared_session_handle(
    context: RunAgentTurnContext<'_>,
    handle: &SessionHandle,
    prepared: PreparedSessionTurn<'_>,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let middleware = MiddlewareRegistry::default();
    run_agent_turn_with_prepared_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(context, &middleware, MiddlewareAgentScope::Main),
        handle,
        PreparedMiddlewareTurn {
            turn: prepared,
            initial_context: Vec::new(),
            event_index: 0,
        },
        handler,
        cancellation,
        controller,
    )
    .await
}

// Compatibility shim for callers that still pass middleware fields separately.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_turn_with_prepared_session_handle_and_middleware(
    context: RunAgentTurnContext<'_>,
    handle: &SessionHandle,
    prepared: PreparedSessionTurn<'_>,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
    middleware: &MiddlewareRegistry,
    agent_scope: MiddlewareAgentScope,
    initial_context: Vec<MiddlewareContextBlock>,
    event_index: usize,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    run_agent_turn_with_prepared_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(context, middleware, agent_scope),
        handle,
        PreparedMiddlewareTurn {
            turn: prepared,
            initial_context,
            event_index,
        },
        handler,
        cancellation,
        controller,
    )
    .await
}

pub async fn run_agent_turn_with_prepared_session_handle_and_middleware_context(
    context: MiddlewareAgentTurnContext<'_>,
    handle: &SessionHandle,
    prepared: PreparedMiddlewareTurn<'_>,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Option<Arc<dyn SubagentController>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let PreparedMiddlewareTurn {
        turn:
            PreparedSessionTurn {
                operation_id,
                turn_id,
                prompt,
                system_prompt,
            },
        initial_context,
        event_index,
    } = prepared;
    let projection = handle.projection().await;
    let mut session = projection_to_legacy_session(&projection);
    session
        .turns
        .retain(|record| record.turn.status != TurnStatus::Running);
    let mut fact_run = SessionFactRun::new(handle, operation_id.clone(), turn_id.clone());
    let result = run_agent_turn_with_optional_controller_and_facts(
        &mut session,
        handler,
        AgentTurnExecution {
            context: context.turn,
            prompt,
            cancellation: cancellation.clone(),
            controller,
            middleware: context.run_config(initial_context, event_index),
            prepared_system_prompt: Some(system_prompt),
        },
        Some(&mut fact_run),
    )
    .await;

    let latest = handle.projection().await;
    if latest
        .turns
        .iter()
        .find(|turn| turn.id == turn_id)
        .is_some_and(|turn| turn.status == SessionTurnStatus::Running)
    {
        let fact = if cancellation.is_cancelled() {
            SessionFact::TurnCancelled {
                reason: "turn cancelled".to_string(),
            }
        } else {
            SessionFact::TurnFailed {
                error: result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .or_else(|| {
                        result
                            .as_ref()
                            .ok()
                            .and_then(|outcome| outcome.error.clone())
                    })
                    .unwrap_or_else(|| "turn ended without a terminal event".to_string()),
            }
        };
        if let Err(error) = handle
            .commit_fact(Some(operation_id), Some(turn_id), fact)
            .await
        {
            handle.replace_operation(None).await;
            return Err(error.into());
        }
    }
    handle.replace_operation(None).await;
    result
}

pub async fn run_agent_turn_with_cancellation(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let middleware = MiddlewareRegistry::default();
    run_agent_turn_with_optional_controller(
        session,
        handler,
        AgentTurnExecution {
            context,
            prompt,
            cancellation,
            controller: None,
            middleware: MiddlewareAgentTurnContext::new(
                context,
                &middleware,
                MiddlewareAgentScope::Main,
            )
            .run_config(Vec::new(), 0),
            prepared_system_prompt: None,
        },
    )
    .await
}

pub async fn run_agent_turn_with_subagent_controller(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
    cancellation: CancellationToken,
    controller: Arc<dyn SubagentController>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let middleware = MiddlewareRegistry::default();
    run_agent_turn_with_optional_controller(
        session,
        handler,
        AgentTurnExecution {
            context,
            prompt,
            cancellation,
            controller: Some(controller),
            middleware: MiddlewareAgentTurnContext::new(
                context,
                &middleware,
                MiddlewareAgentScope::Main,
            )
            .run_config(Vec::new(), 0),
            prepared_system_prompt: None,
        },
    )
    .await
}

async fn run_agent_turn_with_optional_controller(
    session: &mut Session,
    handler: &mut impl TurnEventHandler,
    execution: AgentTurnExecution<'_>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    run_agent_turn_with_optional_controller_and_facts(session, handler, execution, None).await
}

async fn run_agent_turn_with_optional_controller_and_facts(
    session: &mut Session,
    handler: &mut impl TurnEventHandler,
    execution: AgentTurnExecution<'_>,
    fact_run: Option<&mut SessionFactRun<'_>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    // 所有状态先写入草稿；只有整个用例正常收束后才替换调用方持有的 Session。
    let mut draft = session.clone();
    let outcome = run_agent_turn_inner(&mut draft, handler, execution, fact_run).await?;
    *session = draft;
    Ok(outcome)
}

async fn run_agent_turn_inner(
    session: &mut Session,
    handler: &mut impl TurnEventHandler,
    execution: AgentTurnExecution<'_>,
    mut fact_run: Option<&mut SessionFactRun<'_>>,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    let AgentTurnExecution {
        context,
        prompt,
        cancellation,
        controller,
        middleware,
        prepared_system_prompt,
    } = execution;
    let cancellation = &cancellation;
    let MiddlewareRunConfig {
        registry: middleware_registry,
        agent_scope,
        mut initial_context,
        event_index: initial_event_index,
    } = middleware;
    // turn base：prepared 路径沿用写 fact 时的拼装结果；否则现场按
    // 配置层 base + 每轮重读的 AGENTS.md + <environment> 块拼装。
    let turn_base = match prepared_system_prompt {
        Some(prepared) => prepared,
        None => assembled_turn_system_prompt(context).await,
    };
    let writer_lease = controller
        .as_ref()
        .and_then(|controller| controller.writer_lease());
    let artifact_root = SessionStore::for_workspace(context.workspace_root, context.session_name)
        .ok()
        .and_then(|store| store.artifact_root().ok());
    let allow_all_tools;
    let tool_filter = match context.tools {
        Some(tools) => tools,
        None => {
            allow_all_tools = ToolsConfig::default();
            &allow_all_tools
        }
    };
    let build = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = ToolRegistry::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            context.workspace_root,
            context.permissions,
            context.mcp_servers,
            context.mcp_cache,
            writer_lease,
            artifact_root.clone(),
            tool_filter,
            context.auto_approve_workspace_writes,
        ) => Some(result),
    };
    let Some(build) = build else {
        return Ok(record_cancelled_turn(session, prompt, context.model));
    };
    let build = build?;
    let mut tools = build.registry;
    let diagnostics = build.diagnostics;
    let mut persistent_controller_registered = false;
    let shared_model = context.client.shared_clone();
    if let Some(model) = shared_model.as_ref() {
        tools.register_subagent(
            Arc::new(
                RuntimeSubagentExecutor::new(
                    model.clone(),
                    // 临时委派 subagent 沿用 spawn 时（本 turn 拼装）的快照，
                    // 生命周期内不再重读 AGENTS.md。
                    Arc::<str>::from(turn_base.as_str()),
                    Arc::new(context.workspace_root.to_path_buf()),
                )
                .with_middleware_context(
                    Arc::new(middleware_registry.clone()),
                    context.model.clone(),
                    Arc::<str>::from(context.session_name),
                    context.turn_index,
                )
                .with_artifact_root(artifact_root),
            ),
            context.subagent_identities,
        )?;
        if let Some(controller) = controller {
            tools.register_subagent_controller(controller)?;
            persistent_controller_registered = true;
        }
    }
    let effective_system_prompt = effective_turn_system_prompt(
        &turn_base,
        shared_model.is_some(),
        persistent_controller_registered,
    );
    let tool_definitions = tools.definitions();

    let operation_id = fact_run.as_deref().map(|run| run.operation_id.clone());
    let turn_id = fact_run.as_deref().map(|run| run.turn_id.clone());
    let middleware_context =
        middleware_execution_context(context, cancellation, agent_scope, operation_id, turn_id);
    let mut event_index = initial_event_index;

    let previous_summary = session.context.summary.clone();
    let previous_summarized_turns = session.context.summarized_turns;
    if context.context_config.auto_compact {
        let budget = auto_compact_trigger_tokens(context.model_limits, context.context_config);
        let estimate =
            estimate_context_tokens(&effective_system_prompt, session, prompt, &tool_definitions);
        if estimate > budget {
            let pre = middleware_registry
                .runtime()
                .run_pre_compact(PreCompactInput {
                    context: middleware_context.clone(),
                    cause: CompactionCause::Automatic,
                    estimated_tokens: estimate,
                    token_budget: Some(budget),
                    current_summary: session.context.summary.clone(),
                    summarized_turns: session.context.summarized_turns,
                })
                .await;
            let pre_cancelled = pre.cancelled;
            let pre_denied = pre.denied();
            deliver_turn_middleware_events(
                context,
                handler,
                &mut fact_run,
                pre.events,
                &mut event_index,
            )
            .await?;
            if pre_cancelled {
                return Ok(record_cancelled_turn(session, prompt, context.model));
            }
            if !pre_denied {
                let mut compacted = session.clone();
                let compaction = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => None,
                    result = compact_session_with_context(
                        context.client,
                        &mut compacted,
                        context.context_config,
                        &pre.context,
                    ) => Some(result),
                };
                let Some(compaction) = compaction else {
                    return Ok(record_cancelled_turn(session, prompt, context.model));
                };
                let compaction = match compaction {
                    Ok(compaction) => compaction,
                    Err(error) => {
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
                };
                if compaction == CompactionOutcome::Changed {
                    let post = middleware_registry
                        .runtime()
                        .run_post_compact(PostCompactInput {
                            context: middleware_context.clone(),
                            cause: CompactionCause::Automatic,
                            previous_summary: previous_summary.clone(),
                            summary: compacted.context.summary.clone().unwrap_or_default(),
                            summarized_turns: compacted.context.summarized_turns,
                        })
                        .await;
                    let post_cancelled = post.cancelled;
                    let fatal_errors = post.fatal_errors.clone();
                    deliver_turn_middleware_events(
                        context,
                        handler,
                        &mut fact_run,
                        post.events,
                        &mut event_index,
                    )
                    .await?;
                    if post_cancelled {
                        return Ok(record_cancelled_turn(session, prompt, context.model));
                    }
                    if !fatal_errors.is_empty() {
                        let message = format!(
                            "post-compact middleware failed: {}",
                            fatal_errors.join("; ")
                        );
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
                    initial_context.extend(post.context);
                    *session = compacted;
                }
                let compacted_estimate = estimate_context_tokens(
                    &effective_system_prompt,
                    session,
                    prompt,
                    &tool_definitions,
                );
                if compacted_estimate > budget {
                    let message = format!(
                        "context compaction failed: context is still over token budget after compaction ({compacted_estimate} > {budget})"
                    );
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
            }
        }
    }
    if (session.context.summary != previous_summary
        || session.context.summarized_turns != previous_summarized_turns)
        && let Some(run) = fact_run.as_deref_mut()
    {
        run.persist_compaction(session).await?;
    }

    let agent = Agent::with_tools(context.client, effective_system_prompt, &tools)
        .with_middleware(middleware_registry.agent().clone());
    let mut agent_error = None;
    let mut handler_error = None;
    let mut turn_completed = false;

    for diagnostic in diagnostics {
        let envelope = make_event_envelope(
            context.session_name,
            context.workspace_root,
            context.turn_index,
            event_index,
            AgentEvent::Warning(diagnostic),
        );
        event_index += 1;
        if let Some(run) = fact_run.as_deref_mut() {
            run.persist_event(&envelope.event).await?;
        }
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
            .run_turn_with_agent_context(
                &session.active_thread,
                prompt.to_string(),
                AgentRunContext {
                    tool: ToolExecutionContext {
                        cancellation: cancellation.clone(),
                    },
                    middleware: Some(middleware_context),
                    initial_context,
                    context_token_limit: mid_turn_context_token_limit(
                        context.context_config,
                        context.model_limits,
                    ),
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
            if let Some(run) = fact_run.as_deref_mut() {
                run.persist_event(&event).await?;
            }
            match &event {
                AgentEvent::TurnCompleted => {
                    turn_completed = true;
                }
                AgentEvent::Error(message) => {
                    agent_error = Some(message.clone());
                }
                AgentEvent::TurnStarted
                | AgentEvent::ModelCallStarted
                | AgentEvent::MiddlewareStarted(_)
                | AgentEvent::MiddlewareFinished(_)
                | AgentEvent::Warning(_)
                | AgentEvent::ReasoningDelta(_)
                | AgentEvent::TextDelta(_)
                | AgentEvent::ModelMessageCommitted { .. }
                | AgentEvent::AgentMessage(_)
                | AgentEvent::SubagentStarted { .. }
                | AgentEvent::SubagentFinished { .. }
                | AgentEvent::SubagentUpdated(_)
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolCallFinished { .. }
                | AgentEvent::ToolResultCommitted { .. }
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
        origin: AgentEventOrigin::ParentTurn {
            turn_id: None,
            turn_index,
        },
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
