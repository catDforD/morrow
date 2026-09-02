use super::*;

pub(crate) struct StartTurnRequest {
    pub(crate) prompt: String,
    pub(crate) prompt_resolved: bool,
    pub(crate) permission_mode: Option<PermissionMode>,
    pub(crate) model_selection: Option<ModelSelection>,
    pub(crate) resolved_model: Option<ResolvedModel>,
    pub(crate) mcp_servers: Option<Vec<McpServerConfig>>,
    pub(crate) subagent_identities: Option<Vec<SubagentIdentity>>,
    pub(crate) subagent_role_overrides: Option<BTreeMap<SubagentRole, SubagentRoleOverride>>,
    pub(crate) subagent_role_models: Option<BTreeMap<SubagentRole, ResolvedModel>>,
}

pub(crate) async fn start_turn(
    state: AppState,
    session_name: String,
    request: StartTurnRequest,
    tx: broadcast::Sender<ServerMessage>,
) -> Result<(String, String), String> {
    let guard = begin_session_command(&state, &session_name).await?;
    let result = start_turn_inner(state.clone(), session_name.clone(), request, tx).await;
    guard.finish().await;
    result
}

async fn start_turn_inner(
    state: AppState,
    session_name: String,
    request: StartTurnRequest,
    tx: broadcast::Sender<ServerMessage>,
) -> Result<(String, String), String> {
    let StartTurnRequest {
        prompt,
        prompt_resolved,
        permission_mode,
        model_selection,
        resolved_model,
        mcp_servers,
        subagent_identities,
        subagent_role_overrides,
        subagent_role_models,
    } = request;
    if state.inner.shutting_down.load(Ordering::Acquire) {
        return Err("server is shutting down".to_string());
    }
    let prompt = if prompt_resolved {
        prompt
    } else {
        match state
            .inner
            .command_registry
            .resolve(ResolveCommandRequest { input: prompt })
        {
            Ok(resolved) => resolved.prompt,
            Err(error) => return Err(error.to_string()),
        }
    };
    if prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }
    let store = session_store(&state, &session_name).map_err(|error| error.message)?;
    if store.is_archived() {
        return Err(format!(
            "session {session_name:?} is archived; restore it before starting a turn"
        ));
    }

    let cancellation = CancellationToken::new();
    let permissions = requested_permissions(
        state.inner.options.permissions,
        permission_mode,
        state.inner.options.permission_ceiling,
    );
    if running_snapshot(&state, &session_name).await.is_some() {
        return Err("session already has a running turn".to_string());
    }
    let persist_model_selection = resolved_model.is_none();
    let resolved_model = match resolved_model {
        Some(model) => model,
        None => match state
            .inner
            .model_registry
            .resolve_for_turn(&session_name, model_selection)
            .await
        {
            Ok(model) => model,
            Err(error) => return Err(error.to_string()),
        },
    };
    if persist_model_selection
        && let Err(error) = state
            .inner
            .model_registry
            .set_session_selection(&session_name, resolved_model.selection.clone())
            .await
    {
        return Err(error.to_string());
    }
    let subagent_identities = match subagent_identities {
        Some(identities) if identities.len() >= subagent_settings::MIN_SUBAGENT_PROFILES => {
            identities
        }
        Some(_) => {
            return Err(format!(
                "at least {} subagent identities are required",
                subagent_settings::MIN_SUBAGENT_PROFILES
            ));
        }
        None => state.inner.subagent_registry.identities().await,
    };
    let subagent_role_overrides = match subagent_role_overrides {
        Some(overrides) => overrides,
        None => state.inner.subagent_registry.role_overrides().await,
    };
    let hooks = state
        .inner
        .hook_manager
        .load_snapshot()
        .map_err(|error| error.to_string())?;
    let middleware = hooks.registry();
    let supervisor = prepare_subagent_supervisor_with_runtime(SubagentSupervisorPreparation {
        state: &state,
        session_name: &session_name,
        parent_model: &resolved_model,
        parent_permissions: permissions,
        identities: &subagent_identities,
        overrides: subagent_role_overrides,
        supplied_models: subagent_role_models,
        middleware: middleware.clone(),
    })
    .await?;
    let resources = ensure_session_resources(&state, &session_name).await?;
    let session_handle = resources.handle;
    let projection = session_handle.projection().await;
    let turn_index = projection.turns.len();
    let mcp_cache = state.inner.mcp_cache.read().await.clone();
    let hook_mcp_servers = mcp_servers.as_deref().unwrap_or(&[]);
    let mut hook_handler = ServerTurnHandler {
        state: state.clone(),
        session_name: session_name.clone(),
        turn_id: String::new(),
        tx: tx.clone(),
    };
    let prepared = agent_runtime::prepare_session_turn_with_middleware_context(
        agent_runtime::MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &resolved_model.client,
                model: &resolved_model.invocation,
                subagent_identities: &subagent_identities,
                system_prompt: &state.inner.options.system_prompt,
                context_config: state.inner.options.context_config,
                model_limits: resolved_model.limits,
                workspace_root: &state.inner.options.workspace_root,
                workspace_instructions: Some(state.inner.options.workspace_instructions.as_ref()),
                permissions,
                mcp_servers: hook_mcp_servers,
                mcp_cache: mcp_cache.as_ref(),
                tools: Some(&state.inner.options.tools),
                auto_approve_workspace_writes: state.inner.options.auto_approve_workspace_writes,
                session_name: &session_name,
                turn_index,
            },
            middleware.as_ref(),
            agent_protocol::MiddlewareAgentScope::Main,
        ),
        &session_handle,
        &prompt,
        &mut hook_handler,
        &cancellation,
        // 该 turn 随后一定以 `Some(supervisor)` 运行，TurnStarted 需记录含持久
        // subagent guidance 的完整 system prompt。
        true,
    )
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "prompt blocked by middleware".to_string())?;
    let result = {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry(session_name.clone())
            .or_insert_with(SessionRuntime::new);
        if runtime.handle.is_none() {
            runtime.handle = Some(session_handle.clone());
        }
        if runtime.running.is_some() {
            return Err("session already has a running turn".to_string());
        }
        let operation_id = prepared.operation_id;
        let turn_id = prepared.turn_id;
        let system_prompt = prepared.system_prompt;
        let result = (operation_id.clone(), turn_id.clone());
        let state_for_task = state.clone();
        let session_for_task = session_name.clone();
        let turn_for_task = turn_id.clone();
        let cancellation_for_task = cancellation.clone();
        let tx_for_task = tx.clone();
        let handle_for_task = session_handle.clone();
        let worker = tokio::spawn(async move {
            run_turn_task(TurnTaskContext {
                state: state_for_task,
                session_name: session_for_task,
                operation_id,
                turn_id: turn_for_task,
                prompt,
                system_prompt,
                permissions,
                resolved_model,
                mcp_servers,
                subagent_identities,
                supervisor,
                tx: tx_for_task,
                session_handle: handle_for_task,
                cancellation: cancellation_for_task,
                middleware,
                initial_context: prepared.initial_context,
                event_index: prepared.event_index,
            })
            .await;
        });
        let handle = worker.abort_handle();
        let state_for_supervisor = state.clone();
        let session_for_supervisor = session_name.clone();
        let turn_for_supervisor = turn_id.clone();
        let tx_for_supervisor = tx.clone();
        tokio::spawn(supervise_turn_worker(
            state_for_supervisor,
            session_for_supervisor,
            turn_for_supervisor,
            tx_for_supervisor,
            worker,
        ));
        runtime.running = Some(RunningTurn {
            turn_id: turn_id.clone(),
            cancellation,
            handle,
        });
        result
    };
    Ok(result)
}

struct TurnTaskContext {
    state: AppState,
    session_name: String,
    operation_id: String,
    turn_id: String,
    prompt: String,
    /// prepare 阶段写入 `TurnStarted` fact 的 turn base prompt，运行阶段复用。
    system_prompt: String,
    permissions: PermissionProfile,
    resolved_model: ResolvedModel,
    mcp_servers: Option<Vec<McpServerConfig>>,
    subagent_identities: Vec<SubagentIdentity>,
    supervisor: Arc<SubagentSupervisor>,
    tx: broadcast::Sender<ServerMessage>,
    session_handle: Arc<SessionHandle>,
    cancellation: CancellationToken,
    middleware: Arc<agent_runtime::MiddlewareRegistry>,
    initial_context: Vec<agent_runtime::MiddlewareContextBlock>,
    event_index: usize,
}

async fn run_turn_task(context: TurnTaskContext) {
    let tx = context.tx.clone();
    let session_handle = context.session_handle.clone();
    let result = run_turn_task_inner(context).await;
    if let Err(error) = result {
        session_handle
            .notice(format!("turn stopped after runtime error: {error}"))
            .await;
        broadcast_error(&tx, error.to_string());
    }
}

pub(crate) async fn supervise_turn_worker(
    state: AppState,
    session_name: String,
    turn_id: String,
    tx: broadcast::Sender<ServerMessage>,
    worker: tokio::task::JoinHandle<()>,
) {
    if worker.await.is_err_and(|error| error.is_panic()) {
        let handle = {
            let sessions = state.inner.sessions.lock().await;
            sessions
                .get(&session_name)
                .and_then(|runtime| runtime.handle.clone())
        };
        if let Some(handle) = handle {
            let projection = handle.projection().await;
            if let Some(turn) = projection.turns.iter().find(|turn| turn.id == turn_id) {
                let result = handle
                    .commit_fact(
                        Some(turn.operation_id.clone()),
                        Some(turn.id.clone()),
                        agent_protocol::SessionFact::TurnInterrupted {
                            reason: "turn worker panicked".to_string(),
                        },
                    )
                    .await;
                handle.replace_operation(None).await;
                if let Err(error) = result {
                    handle
                        .notice(format!("failed to record panicked turn: {error}"))
                        .await;
                }
            }
        }
        broadcast_error(&tx, format!("turn {turn_id} worker panicked"));
    }
    cancel_matching_approvals(&state, &session_name, &tx, |request| {
        matches!(
            &request.origin,
            ApprovalOrigin::ParentTurn {
                turn_id: Some(pending_turn),
                ..
            } if pending_turn == &turn_id
        )
    })
    .await;
    // 无论正常返回、panic 还是 abort，JoinHandle 完成都表示 worker future 已被 drop。
    clear_running_turn(&state, &session_name, &turn_id).await;
}

async fn run_turn_task_inner(context: TurnTaskContext) -> Result<(), agent_runtime::RuntimeError> {
    let TurnTaskContext {
        state,
        session_name,
        operation_id,
        turn_id,
        prompt,
        system_prompt,
        permissions,
        resolved_model,
        mcp_servers,
        subagent_identities,
        supervisor,
        tx,
        session_handle,
        cancellation,
        middleware,
        initial_context,
        event_index,
    } = context;
    let options = state.inner.options.clone();
    let mcp_cache = state.inner.mcp_cache.read().await.clone();
    let mcp_servers = match mcp_servers {
        Some(servers) => servers,
        None => state.inner.mcp_registry.effective_servers().await,
    };
    let projection = session_handle.projection().await;
    let turn_index = projection
        .turns
        .iter()
        .position(|turn| turn.id == turn_id)
        .unwrap_or_else(|| projection.turns.len().saturating_sub(1));
    let mut handler = ServerTurnHandler {
        state: state.clone(),
        session_name: session_name.clone(),
        turn_id: turn_id.clone(),
        tx: tx.clone(),
    };

    let outcome =
        agent_runtime::run_agent_turn_with_prepared_session_handle_and_middleware_context(
            agent_runtime::MiddlewareAgentTurnContext::new(
                RunAgentTurnContext {
                    client: &resolved_model.client,
                    model: &resolved_model.invocation,
                    subagent_identities: &subagent_identities,
                    system_prompt: &options.system_prompt,
                    context_config: options.context_config,
                    model_limits: resolved_model.limits,
                    workspace_root: &options.workspace_root,
                    workspace_instructions: Some(options.workspace_instructions.as_ref()),
                    permissions,
                    mcp_servers: &mcp_servers,
                    mcp_cache: mcp_cache.as_ref(),
                    tools: Some(&options.tools),
                    auto_approve_workspace_writes: options.auto_approve_workspace_writes,
                    session_name: &session_name,
                    turn_index,
                },
                middleware.as_ref(),
                agent_protocol::MiddlewareAgentScope::Main,
            ),
            &session_handle,
            agent_runtime::PreparedMiddlewareTurn {
                turn: agent_runtime::PreparedSessionTurn {
                    operation_id,
                    turn_id: turn_id.clone(),
                    prompt: &prompt,
                    system_prompt,
                },
                initial_context,
                event_index,
            },
            &mut handler,
            cancellation,
            Some(supervisor),
        )
        .await?;

    if let Some(error) = outcome.error {
        broadcast_error(&tx, error);
    }

    Ok(())
}

pub(crate) async fn resolve_approval(
    state: &AppState,
    session_name: &str,
    request_id: String,
    approved: bool,
    tx: &broadcast::Sender<ServerMessage>,
) {
    let pending = {
        let mut sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get_mut(session_name) else {
            broadcast_error(tx, "session has no pending approval");
            return;
        };
        let Some(front) = runtime.approvals.front() else {
            broadcast_error(tx, "session has no pending approval");
            return;
        };
        if front.request.id != request_id {
            let expected = front.request.id.clone();
            let queued = runtime
                .approvals
                .iter()
                .any(|approval| approval.request.id == request_id);
            broadcast_error(
                tx,
                if queued {
                    format!("approval {request_id} is queued behind current approval {expected}")
                } else {
                    format!(
                        "approval decision {request_id} does not match pending approval {expected}"
                    )
                },
            );
            return;
        }
        runtime
            .approvals
            .pop_front()
            .expect("approval queue front checked")
    };

    let _ = pending.sender.send(if approved {
        ApprovalDecision::approve(request_id)
    } else {
        ApprovalDecision::deny(request_id)
    });
    broadcast_approval_queue(state, session_name, tx).await;
}

pub(crate) async fn enqueue_approval(
    state: &AppState,
    session_name: &str,
    request: ApprovalRequest,
    tx: &broadcast::Sender<ServerMessage>,
) -> Result<ApprovalDecision, String> {
    let request_id = request.id.clone();
    let (sender, receiver) = oneshot::channel();
    {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .get_mut(session_name)
            .ok_or_else(|| "session state disappeared".to_string())?;
        if runtime
            .approvals
            .iter()
            .any(|approval| approval.request.id == request_id)
        {
            return Err(format!("approval request {request_id:?} is already queued"));
        }
        runtime
            .approvals
            .push_back(PendingApproval { request, sender });
    }
    broadcast_approval_queue(state, session_name, tx).await;
    match receiver.await {
        Ok(decision) => Ok(decision),
        Err(_) => Ok(ApprovalDecision::deny(request_id)),
    }
}

pub(crate) async fn cancel_matching_approvals(
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
    matches: impl Fn(&ApprovalRequest) -> bool,
) {
    let removed = {
        let mut sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get_mut(session_name) else {
            return;
        };
        let mut kept = VecDeque::with_capacity(runtime.approvals.len());
        let mut removed = Vec::new();
        while let Some(approval) = runtime.approvals.pop_front() {
            if matches(&approval.request) {
                removed.push(approval);
            } else {
                kept.push_back(approval);
            }
        }
        runtime.approvals = kept;
        removed
    };
    if removed.is_empty() {
        return;
    }
    for pending in removed {
        let _ = pending
            .sender
            .send(ApprovalDecision::deny(pending.request.id));
    }
    broadcast_approval_queue(state, session_name, tx).await;
}

async fn broadcast_approval_queue(
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
) {
    let approvals = approval_snapshots(state, session_name).await;
    broadcast_message(
        tx,
        ServerMessage::ApprovalQueueUpdated {
            approvals: approvals.clone(),
        },
    );
    if let Ok(resources) = ensure_session_resources(state, session_name).await {
        resources.handle.set_approvals(approvals).await;
    }
}

pub(crate) async fn cancel_turn(
    state: &AppState,
    session_name: &str,
    turn_id: String,
    tx: &broadcast::Sender<ServerMessage>,
) {
    let cancellation = {
        let sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get(session_name) else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        let Some(running) = runtime.running.as_ref() else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        if running.turn_id != turn_id {
            broadcast_error(tx, format!("turn {turn_id} is not running"));
            return;
        }
        running.cancellation.clone()
    };

    cancellation.cancel();
    cancel_matching_approvals(state, session_name, tx, |request| {
        matches!(
            &request.origin,
            ApprovalOrigin::ParentTurn {
                turn_id: Some(pending_turn),
                ..
            } if pending_turn == &turn_id
        )
    })
    .await;

    // 正常路径由 runtime 收束失败 Turn。只有长期不退出时才使用 abort 兜底。
    let state = state.clone();
    let session_name = session_name.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let handle = {
            let sessions = state.inner.sessions.lock().await;
            sessions
                .get(&session_name)
                .and_then(|runtime| runtime.running.as_ref())
                .filter(|running| running.turn_id == turn_id && running.cancellation.is_cancelled())
                .map(|running| running.handle.clone())
        };
        if let Some(handle) = handle {
            handle.abort();
            // `abort` 只发送终止请求。等待任务真正结束，确保其 future（以及工具清理
            // guard）已被 drop 后，才允许同一 Session 接受下一轮请求。
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
            clear_running_turn(&state, &session_name, &turn_id).await;
            broadcast_error(&tx, format!("turn {turn_id} cancellation timed out"));
        }
    });
}

pub(crate) async fn clear_running_turn(state: &AppState, session_name: &str, turn_id: &str) {
    let mut sessions = state.inner.sessions.lock().await;
    if let Some(runtime) = sessions.get_mut(session_name)
        && runtime
            .running
            .as_ref()
            .is_some_and(|running| running.turn_id == turn_id)
    {
        runtime.running = None;
        release_idle_session_handle(runtime);
    }
}

pub(crate) async fn reset_mcp_cache(state: &AppState) {
    let previous = {
        let mut current = state.inner.mcp_cache.write().await;
        std::mem::replace(&mut *current, Arc::new(McpToolCache::new()))
    };
    previous.clear().await;
}

struct ServerTurnHandler {
    state: AppState,
    session_name: String,
    turn_id: String,
    tx: broadcast::Sender<ServerMessage>,
}

impl TurnEventHandler for ServerTurnHandler {
    fn on_event(
        &mut self,
        envelope: &AgentEventEnvelope,
    ) -> Result<(), agent_runtime::RuntimeError> {
        let mut envelope = envelope.clone();
        envelope.origin = AgentEventOrigin::ParentTurn {
            turn_id: Some(self.turn_id.clone()),
            turn_index: envelope.turn_index,
        };
        if let AgentEvent::ApprovalRequested(request) = &mut envelope.event {
            *request = parent_approval_request(request, &self.turn_id);
        }
        broadcast_message(&self.tx, ServerMessage::AgentEvent(Box::new(envelope)));
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a agent_protocol::ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, agent_runtime::RuntimeError>> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let turn_id = self.turn_id.clone();
        let request = parent_approval_request(request, &turn_id);
        let tx = self.tx.clone();

        async move {
            {
                let sessions = state.inner.sessions.lock().await;
                let runtime = sessions.get(&session_name).ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("session state disappeared")
                })?;
                let running = runtime.running.as_ref().ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("running turn disappeared")
                })?;
                if running.turn_id != turn_id {
                    return Err(agent_runtime::RuntimeError::event_handler(
                        "running turn changed while waiting for approval",
                    ));
                }
            }
            enqueue_approval(&state, &session_name, request, &tx)
                .await
                .map_err(agent_runtime::RuntimeError::event_handler)
        }
        .boxed()
    }
}

fn parent_approval_request(request: &ApprovalRequest, turn_id: &str) -> ApprovalRequest {
    let mut request = request.clone();
    request.origin = ApprovalOrigin::ParentTurn {
        turn_id: Some(turn_id.to_string()),
        tool_call_id: request.id.strip_prefix("approval-").map(str::to_string),
    };
    request
}

pub(crate) struct ServerSubagentObserver {
    pub(crate) state: Weak<ServerState>,
    pub(crate) session_name: String,
    pub(crate) tx: broadcast::Sender<ServerMessage>,
}

impl SubagentObserver for ServerSubagentObserver {
    fn on_event(&self, event: &AgentEventEnvelope) {
        broadcast_message(&self.tx, ServerMessage::AgentEvent(Box::new(event.clone())));
        if let AgentEvent::SubagentUpdated(snapshot) = &event.event {
            let state = self.state.clone();
            let session_name = self.session_name.clone();
            let snapshot = (**snapshot).clone();
            tokio::spawn(async move {
                if let Some(inner) = state.upgrade() {
                    let handle = inner
                        .sessions
                        .lock()
                        .await
                        .get(&session_name)
                        .and_then(|runtime| runtime.handle.clone());
                    if let Some(handle) = handle {
                        handle.upsert_subagent(snapshot).await;
                    }
                }
            });
        }
    }

    fn resolve_approval(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'static, Result<ApprovalDecision, String>> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let tx = self.tx.clone();
        async move {
            let Some(inner) = state.upgrade() else {
                return Ok(ApprovalDecision::deny(request.id));
            };
            enqueue_approval(&WorkspaceService { inner }, &session_name, request, &tx).await
        }
        .boxed()
    }

    fn cancel_approvals(
        &self,
        instance_id: String,
        run_id: Option<String>,
    ) -> BoxFuture<'static, ()> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let tx = self.tx.clone();
        async move {
            let Some(inner) = state.upgrade() else {
                return;
            };
            cancel_matching_approvals(&WorkspaceService { inner }, &session_name, &tx, |request| {
                match &request.origin {
                    ApprovalOrigin::SubagentRun {
                        instance_id: pending_instance,
                        run_id: pending_run,
                        ..
                    } => {
                        pending_instance == &instance_id
                            && run_id.as_ref().is_none_or(|run_id| pending_run == run_id)
                    }
                    _ => false,
                }
            })
            .await;
        }
        .boxed()
    }
}

pub(crate) fn requested_permissions(
    default: PermissionProfile,
    requested_mode: Option<PermissionMode>,
    ceiling: PermissionMode,
) -> PermissionProfile {
    match requested_mode {
        // Derive the profile from the clamped mode so a reduced mode cannot
        // keep the escalated shell policy of the requested one.
        Some(mode) => PermissionProfile::for_mode(mode.clamp(ceiling)),
        None => {
            let mode = default.mode.clamp(ceiling);
            let shell = match (default.shell, mode) {
                (ShellPolicy::Allow, PermissionMode::DangerFullAccess) => ShellPolicy::Allow,
                (ShellPolicy::Allow, _) => ShellPolicy::Prompt,
                (shell, _) => shell,
            };
            PermissionProfile { mode, shell }
        }
    }
}

async fn prepare_subagent_supervisor(
    state: &AppState,
    session_name: &str,
    parent_model: &ResolvedModel,
    parent_permissions: PermissionProfile,
    identities: &[SubagentIdentity],
) -> Result<Arc<SubagentSupervisor>, String> {
    let overrides = state.inner.subagent_registry.role_overrides().await;
    let middleware = state
        .inner
        .hook_manager
        .load_snapshot()
        .map_err(|error| error.to_string())?
        .registry();
    prepare_subagent_supervisor_with_runtime(SubagentSupervisorPreparation {
        state,
        session_name,
        parent_model,
        parent_permissions,
        identities,
        overrides,
        supplied_models: None,
        middleware,
    })
    .await
}

pub(crate) struct SubagentSupervisorPreparation<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) session_name: &'a str,
    pub(crate) parent_model: &'a ResolvedModel,
    pub(crate) parent_permissions: PermissionProfile,
    pub(crate) identities: &'a [SubagentIdentity],
    pub(crate) overrides: BTreeMap<SubagentRole, SubagentRoleOverride>,
    pub(crate) supplied_models: Option<BTreeMap<SubagentRole, ResolvedModel>>,
    pub(crate) middleware: Arc<agent_runtime::MiddlewareRegistry>,
}

pub(crate) async fn prepare_subagent_supervisor_with_runtime(
    preparation: SubagentSupervisorPreparation<'_>,
) -> Result<Arc<SubagentSupervisor>, String> {
    let SubagentSupervisorPreparation {
        state,
        session_name,
        parent_model,
        parent_permissions,
        identities,
        overrides,
        mut supplied_models,
        middleware,
    } = preparation;
    let resources = ensure_session_resources(state, session_name).await?;
    // 持久 subagent 的 base 在每次 turn 准备时经缓存重拼（spawn 时快照），
    // 实例生命周期内沿用 spawn 时的 prompt，不再重读 AGENTS.md。
    let subagent_base_prompt: Arc<str> = Arc::from(
        state
            .inner
            .options
            .workspace_instructions
            .apply(&state.inner.options.system_prompt),
    );
    let mut roles = BTreeMap::new();
    for role in SubagentRole::ALL {
        let role_config = overrides.get(&role).cloned().unwrap_or_default();
        let resolved = match supplied_models
            .as_mut()
            .and_then(|models| models.remove(&role))
        {
            Some(resolved) => resolved,
            None => match role_config.model_selection.clone() {
                Some(selection) if selection != parent_model.selection => state
                    .inner
                    .model_registry
                    .resolve_for_turn(session_name, Some(selection))
                    .await
                    .map_err(|error| {
                        format!("{} subagent model is unavailable: {error}", role.as_str())
                    })?,
                _ => parent_model.clone(),
            },
        };
        let model: Arc<dyn Model> = Arc::new(resolved.client.clone());
        roles.insert(
            role,
            SubagentRoleRuntime {
                model,
                invocation: resolved.invocation,
                limits: resolved.limits,
                role_config,
                base_system_prompt: subagent_base_prompt.clone(),
                parent_permissions,
                middleware: middleware.clone(),
            },
        );
    }
    resources
        .supervisor
        .update_runtime(roles, identities.to_vec())
        .await;
    for invocation in resources.supervisor.required_models().await {
        let selection = ModelSelection {
            provider_id: invocation.provider_id.clone(),
            model_id: invocation.model_id.clone(),
            reasoning: invocation.reasoning,
        };
        if let Ok(resolved) = state
            .inner
            .model_registry
            .resolve_for_turn(session_name, Some(selection))
            .await
        {
            let model: Arc<dyn Model> = Arc::new(resolved.client.clone());
            resources
                .supervisor
                .register_model_runtime(model, resolved.invocation, resolved.limits)
                .await;
        }
    }
    Ok(resources.supervisor)
}

pub(crate) async fn prepare_direct_subagent_supervisor(
    state: &AppState,
    session_name: &str,
) -> Result<Arc<SubagentSupervisor>, String> {
    let parent_model = state
        .inner
        .model_registry
        .resolve_for_turn(session_name, None)
        .await
        .map_err(|error| error.to_string())?;
    let identities = state.inner.subagent_registry.identities().await;
    prepare_subagent_supervisor(
        state,
        session_name,
        &parent_model,
        state.inner.options.permissions,
        &identities,
    )
    .await
}
