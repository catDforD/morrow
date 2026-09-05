use super::*;

pub(crate) const SESSION_DIRECTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone)]
pub struct ServerOptions {
    pub host: IpAddr,
    pub port: u16,
    pub fallback_model: Option<FallbackModel>,
    pub model_store_path: PathBuf,
    pub mcp_store_path: PathBuf,
    pub command_store_path: PathBuf,
    pub subagent_store_path: PathBuf,
    pub hook_home_dir: PathBuf,
    /// 配置层 base prompt（不含 AGENTS.md）；AGENTS.md 段落每个 turn 经
    /// `workspace_instructions` 缓存重读后再拼接。
    pub system_prompt: String,
    /// 每轮重读 AGENTS.md 的进程级缓存（mtime 未变时零文件读取）。
    pub workspace_instructions: Arc<WorkspaceInstructionsCache>,
    pub context_config: ContextConfig,
    pub workspace_root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub config_diagnostics: Vec<String>,
    /// Default for legacy clients that do not select a permission mode per turn.
    pub permissions: PermissionProfile,
    /// workspace_write 模式下 workspace 内文件变更是否自动放行（配置回退开关的取反值）。
    pub auto_approve_workspace_writes: bool,
    /// Cap on the permission mode web clients may request per turn.
    pub permission_ceiling: PermissionMode,
    pub mcp_servers: Vec<McpServerConfig>,
    pub tools: ToolsConfig,
    pub default_session_name: String,
}

pub fn server_options_from_loaded_config(
    host: IpAddr,
    port: u16,
    workspace_root: PathBuf,
    home: &std::path::Path,
    loaded: LoadedServerConfig,
    default_session_name: String,
) -> Result<ServerOptions, ModelError> {
    // 冷启动预热：收集 AGENTS.md 诊断并填充缓存；之后每个 turn 经缓存重读。
    let workspace_instructions = Arc::new(WorkspaceInstructionsCache::new(&workspace_root));
    let mut config_diagnostics = loaded.diagnostics;
    config_diagnostics.extend(workspace_instructions.prewarm());
    let fallback_model = loaded
        .model
        .map(|model| {
            let model_name = model.config.model.clone();
            let limits = model.config.context_limits();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: model.config.base_url,
                model: model_name.clone(),
                api_key: model.api_key,
                timeout: Duration::from_secs(model.config.timeout_secs),
                max_retries: model.config.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            })?;
            Ok(FallbackModel {
                provider_name: "默认配置".to_string(),
                model_id: model_name.clone(),
                model_name: model_name.clone(),
                client: Some(client),
                limits,
                reasoning_profile: reasoning_profile(&model_name),
            })
        })
        .transpose()?;

    Ok(ServerOptions {
        host,
        port,
        fallback_model,
        model_store_path: home.join(".morrow").join("web-models.json"),
        mcp_store_path: home.join(".morrow").join("web-mcp.json"),
        command_store_path: home.join(".morrow").join("commands"),
        subagent_store_path: home.join(".morrow").join("subagents.json"),
        hook_home_dir: home.to_path_buf(),
        system_prompt: loaded.config.agent.system_prompt,
        workspace_instructions,
        context_config: loaded.config.context,
        workspace_root,
        config_path: loaded.path,
        config_diagnostics,
        permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
        auto_approve_workspace_writes: !loaded.config.workspace_write_require_approval,
        permission_ceiling: loaded.config.server.permission_ceiling,
        mcp_servers: loaded.config.mcp_servers,
        tools: loaded.config.tools,
        default_session_name,
    })
}

fn reasoning_profile(model: &str) -> ReasoningProfile {
    match model {
        "deepseek-v4-flash" | "deepseek-v4-pro" => ReasoningProfile::Deepseek,
        _ => ReasoningProfile::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerActivity {
    pub running_turns: usize,
    pub pending_approvals: usize,
}

impl ServerActivity {
    pub fn is_idle(self) -> bool {
        self.running_turns == 0
    }
}

#[derive(Clone)]
pub struct WorkspaceService {
    pub(crate) inner: Arc<ServerState>,
}

pub(crate) type AppState = WorkspaceService;

pub(crate) struct ServerState {
    pub(crate) options: ServerOptions,
    pub(crate) model_registry: ModelRegistry,
    pub(crate) mcp_registry: McpRegistry,
    pub(crate) command_registry: CommandRegistry,
    pub(crate) hook_manager: HookManager,
    pub(crate) subagent_registry: SubagentRegistry,
    pub(crate) sessions: Mutex<HashMap<String, SessionRuntime>>,
    pub(crate) mcp_cache: RwLock<Arc<McpToolCache>>,
    pub(crate) access_policy: ServerAccessPolicy,
    pub(crate) shutting_down: AtomicBool,
}

pub(crate) async fn server_activity(state: &AppState) -> ServerActivity {
    let (mut running_turns, pending_approvals, supervisors) = {
        let sessions = state.inner.sessions.lock().await;
        let running_turns = sessions
            .values()
            .filter(|runtime| runtime.running.is_some())
            .count();
        let pending_approvals = sessions
            .values()
            .map(|runtime| runtime.approvals.len())
            .sum();
        let supervisors = sessions
            .values()
            .filter_map(|runtime| runtime.supervisor.clone())
            .collect::<Vec<_>>();
        (running_turns, pending_approvals, supervisors)
    };
    for supervisor in supervisors {
        running_turns += supervisor.active_run_count().await;
    }
    ServerActivity {
        running_turns,
        pending_approvals,
    }
}

pub(crate) async fn cancel_all_turns(state: &AppState, timeout: Duration) {
    let (handles, supervisors, approvals) = {
        let mut sessions = state.inner.sessions.lock().await;
        let handles = sessions
            .values()
            .filter_map(|runtime| runtime.running.as_ref())
            .map(|running| {
                running.cancellation.cancel();
                running.handle.clone()
            })
            .collect::<Vec<_>>();
        let supervisors = sessions
            .values()
            .filter_map(|runtime| runtime.supervisor.clone())
            .collect::<Vec<_>>();
        let approvals = sessions
            .values_mut()
            .flat_map(|runtime| runtime.approvals.drain(..))
            .collect::<Vec<_>>();
        (handles, supervisors, approvals)
    };
    for pending in approvals {
        let _ = pending
            .sender
            .send(ApprovalDecision::deny(pending.request.id));
    }
    for supervisor in &supervisors {
        for snapshot in supervisor.snapshots().await {
            if snapshot.status.is_active() {
                let _ = supervisor.cancel(snapshot.id).await;
            }
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let parent_active = handles.iter().any(|handle| !handle.is_finished());
        let mut subagent_active = false;
        for supervisor in &supervisors {
            subagent_active |= supervisor.has_active_runs().await;
        }
        if (!parent_active && !subagent_active) || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for handle in handles.iter().filter(|handle| !handle.is_finished()) {
        handle.abort();
    }
    while handles.iter().any(|handle| !handle.is_finished()) {
        tokio::task::yield_now().await;
    }

    let mut sessions = state.inner.sessions.lock().await;
    for runtime in sessions.values_mut() {
        if runtime.running.is_some() {
            runtime.running = None;
        }
    }
}

pub(crate) struct SessionRuntime {
    pub(crate) tx: broadcast::Sender<ServerMessage>,
    pub(crate) handle: Option<Arc<SessionHandle>>,
    pub(crate) running: Option<RunningTurn>,
    pub(crate) approvals: VecDeque<PendingApproval>,
    pub(crate) supervisor: Option<Arc<SubagentSupervisor>>,
    pub(crate) writer_lease: Arc<Semaphore>,
    pub(crate) subscribers: usize,
    pub(crate) active_commands: usize,
    pub(crate) lifecycle_mutation: bool,
}

pub(crate) struct RunningTurn {
    pub(crate) turn_id: String,
    pub(crate) cancellation: CancellationToken,
    pub(crate) handle: AbortHandle,
}

pub(crate) struct PendingApproval {
    pub(crate) request: ApprovalRequest,
    pub(crate) sender: oneshot::Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningTurnSnapshot {
    pub(crate) turn_id: String,
    pub(crate) pending_approval: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentTranscriptSnapshot {
    instance: SubagentInstanceSnapshot,
    model: agent_protocol::ModelInvocation,
    permission_ceiling: PermissionProfile,
    role_config: SubagentRoleOverride,
    session: SessionProjection,
    runs: Vec<SubagentRunRecord>,
    events: Vec<AgentEventEnvelope>,
}

impl SubagentTranscriptSnapshot {
    pub(crate) fn from_document(
        document: SubagentInstanceDocument,
        session: SessionProjection,
        events: Vec<AgentEventEnvelope>,
    ) -> Self {
        Self {
            instance: document.snapshot,
            model: document.model,
            permission_ceiling: document.permission_ceiling,
            role_config: document.role_config,
            session,
            runs: document.runs,
            events,
        }
    }
}

impl SessionRuntime {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            handle: None,
            running: None,
            approvals: VecDeque::new(),
            supervisor: None,
            writer_lease: Arc::new(Semaphore::new(1)),
            subscribers: 0,
            active_commands: 0,
            lifecycle_mutation: false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionResources {
    pub(crate) tx: broadcast::Sender<ServerMessage>,
    pub(crate) handle: Arc<SessionHandle>,
    pub(crate) supervisor: Arc<SubagentSupervisor>,
}

pub(crate) async fn ensure_session_resources(
    state: &AppState,
    session_name: &str,
) -> Result<SessionResources, String> {
    let identities = state.inner.subagent_registry.identities().await;
    let mut sessions = state.inner.sessions.lock().await;
    let runtime = sessions
        .entry(session_name.to_string())
        .or_insert_with(SessionRuntime::new);
    if runtime.lifecycle_mutation {
        return Err("session lifecycle mutation is in progress".to_string());
    }
    if runtime.handle.is_none() {
        let store = SessionStore::for_workspace(&state.inner.options.workspace_root, session_name)
            .map_err(|error| error.to_string())?;
        runtime.handle = Some(Arc::new(
            SessionHandle::open_existing(
                store,
                session_name.to_string(),
                state.inner.options.permissions,
            )
            .map_err(|error| error.to_string())?,
        ));
    }
    if runtime.supervisor.is_none() {
        let observer = Arc::new(ServerSubagentObserver {
            state: Arc::downgrade(&state.inner),
            session_name: session_name.to_string(),
            tx: runtime.tx.clone(),
        });
        let supervisor = SubagentSupervisor::new_with_writer_lease(
            state.inner.options.workspace_root.clone(),
            session_name.to_string(),
            state.inner.options.context_config,
            BTreeMap::new(),
            identities,
            observer,
            runtime.writer_lease.clone(),
            state.inner.options.tools.clone(),
        )
        .map_err(|error| error.to_string())?;
        runtime.supervisor = Some(Arc::new(supervisor));
    }
    Ok(SessionResources {
        tx: runtime.tx.clone(),
        handle: runtime
            .handle
            .as_ref()
            .expect("session handle initialized")
            .clone(),
        supervisor: runtime
            .supervisor
            .as_ref()
            .expect("subagent supervisor initialized")
            .clone(),
    })
}

pub(crate) async fn register_session_subscription(
    state: &AppState,
    session_name: &str,
) -> Result<SessionResources, String> {
    let resources = ensure_session_resources(state, session_name).await?;
    let mut sessions = state.inner.sessions.lock().await;
    let runtime = sessions
        .entry(session_name.to_string())
        .or_insert_with(SessionRuntime::new);
    if runtime.lifecycle_mutation {
        return Err("session lifecycle mutation is in progress".to_string());
    }
    if runtime.handle.is_none() {
        runtime.handle = Some(resources.handle.clone());
    }
    runtime.subscribers += 1;
    Ok(resources)
}

pub(crate) async fn release_session_subscription(state: &AppState, session_name: &str) {
    let mut sessions = state.inner.sessions.lock().await;
    let Some(runtime) = sessions.get_mut(session_name) else {
        return;
    };
    runtime.subscribers = runtime.subscribers.saturating_sub(1);
    release_idle_session_handle(runtime);
}

pub(crate) fn release_idle_session_handle(runtime: &mut SessionRuntime) {
    if runtime.subscribers == 0 && runtime.running.is_none() {
        runtime.handle = None;
    }
}

#[cfg(test)]
pub(crate) async fn session_sender(
    state: &AppState,
    session_name: &str,
) -> broadcast::Sender<ServerMessage> {
    let mut sessions = state.inner.sessions.lock().await;
    sessions
        .entry(session_name.to_string())
        .or_insert_with(SessionRuntime::new)
        .tx
        .clone()
}

pub(crate) async fn running_snapshot(
    state: &AppState,
    session_name: &str,
) -> Option<RunningTurnSnapshot> {
    let sessions = state.inner.sessions.lock().await;
    sessions
        .get(session_name)
        .and_then(|runtime| runtime.running.as_ref())
        .map(|running| RunningTurnSnapshot {
            turn_id: running.turn_id.clone(),
            pending_approval: sessions
                .get(session_name)
                .and_then(|runtime| runtime.approvals.front())
                .map(|approval| approval.request.id.clone()),
        })
}

pub(crate) async fn session_has_active_work(state: &AppState, session_name: &str) -> bool {
    let supervisor = {
        let sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get(session_name) else {
            return false;
        };
        if runtime.running.is_some()
            || !runtime.approvals.is_empty()
            || runtime.active_commands > 0
            || runtime.lifecycle_mutation
        {
            return true;
        }
        runtime.supervisor.clone()
    };
    match supervisor {
        Some(supervisor) => supervisor.has_active_runs().await,
        None => false,
    }
}

pub(crate) struct SessionCommandGuard {
    state: AppState,
    session_name: String,
    finished: bool,
}

impl SessionCommandGuard {
    pub(crate) async fn finish(mut self) {
        end_session_command(&self.state, &self.session_name).await;
        self.finished = true;
    }
}

impl Drop for SessionCommandGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                end_session_command(&state, &session_name).await;
            });
        }
    }
}

pub(crate) async fn begin_session_command(
    state: &AppState,
    session_name: &str,
) -> Result<SessionCommandGuard, String> {
    let mut sessions = state.inner.sessions.lock().await;
    let runtime = sessions
        .entry(session_name.to_string())
        .or_insert_with(SessionRuntime::new);
    if runtime.lifecycle_mutation {
        return Err("session lifecycle mutation is in progress".to_string());
    }
    runtime.active_commands += 1;
    Ok(SessionCommandGuard {
        state: state.clone(),
        session_name: session_name.to_string(),
        finished: false,
    })
}

async fn end_session_command(state: &AppState, session_name: &str) {
    let mut sessions = state.inner.sessions.lock().await;
    let Some(runtime) = sessions.get_mut(session_name) else {
        return;
    };
    runtime.active_commands = runtime.active_commands.saturating_sub(1);
    release_idle_session_handle(runtime);
}

pub(crate) async fn with_session_command<T>(
    state: &AppState,
    session_name: &str,
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let guard = begin_session_command(state, session_name).await?;
    let result = operation.await;
    guard.finish().await;
    result
}

pub(crate) async fn begin_session_lifecycle(
    state: &AppState,
    session_name: &str,
) -> Result<Option<Arc<SessionHandle>>, ApiError> {
    let (handle, supervisor) = {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry(session_name.to_string())
            .or_insert_with(SessionRuntime::new);
        if runtime.lifecycle_mutation
            || runtime.active_commands > 0
            || runtime.running.is_some()
            || !runtime.approvals.is_empty()
        {
            return Err(ApiError::conflict("session has active agent work"));
        }
        runtime.lifecycle_mutation = true;
        (runtime.handle.clone(), runtime.supervisor.clone())
    };
    if let Some(supervisor) = supervisor
        && supervisor.has_active_runs().await
    {
        finish_session_lifecycle(state, session_name).await;
        return Err(ApiError::conflict("session has active agent work"));
    }
    Ok(handle)
}

pub(crate) async fn finish_session_lifecycle(state: &AppState, session_name: &str) {
    let mut sessions = state.inner.sessions.lock().await;
    if let Some(runtime) = sessions.get_mut(session_name) {
        runtime.lifecycle_mutation = false;
        release_idle_session_handle(runtime);
    }
}

pub(crate) async fn approval_snapshots(
    state: &AppState,
    session_name: &str,
) -> Vec<ApprovalRequest> {
    state
        .inner
        .sessions
        .lock()
        .await
        .get(session_name)
        .map(|runtime| {
            runtime
                .approvals
                .iter()
                .map(|approval| approval.request.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn session_store(state: &AppState, name: &str) -> Result<SessionStore, ApiError> {
    SessionStore::for_workspace(&state.inner.options.workspace_root, name)
        .map_err(|error| ApiError::bad_request(error.to_string()))
}

pub(crate) fn require_active_session(
    state: &AppState,
    name: &str,
) -> Result<SessionStore, ApiError> {
    let store = session_store(state, name)?;
    reject_archived_session(&store, name)?;
    store.load_projection().map_err(|error| match error {
        agent_runtime::SessionStoreError::SessionNotFound { .. } => {
            ApiError::not_found(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    })?;
    Ok(store)
}

fn reject_archived_session(store: &SessionStore, name: &str) -> Result<(), ApiError> {
    if store.is_archived() {
        return Err(ApiError::conflict(format!(
            "session {name:?} is archived; restore it before opening it"
        )));
    }
    Ok(())
}

pub(crate) fn session_mutation_error(error: agent_runtime::SessionStoreError) -> ApiError {
    match error {
        agent_runtime::SessionStoreError::SessionNotFound { .. }
        | agent_runtime::SessionStoreError::TargetExists { .. } => {
            ApiError::conflict(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}

pub(crate) fn model_registry_error(error: ModelRegistryError) -> ApiError {
    match error {
        ModelRegistryError::Conflict(_) | ModelRegistryError::SelectionUnavailable(_) => {
            ApiError::conflict(error.to_string())
        }
        ModelRegistryError::Validation(_) | ModelRegistryError::ProviderNotFound(_) => {
            ApiError::bad_request(error.to_string())
        }
        ModelRegistryError::Model(ModelError::HttpStatus { .. })
        | ModelRegistryError::Model(ModelError::Request(_)) => {
            ApiError::bad_request(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}

pub(crate) fn mcp_registry_error(error: McpRegistryError) -> ApiError {
    match error {
        McpRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        McpRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        McpRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}

pub(crate) fn command_registry_error(error: CommandRegistryError) -> ApiError {
    match error {
        CommandRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        CommandRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        CommandRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}

pub(crate) fn subagent_registry_error(error: SubagentRegistryError) -> ApiError {
    match error {
        SubagentRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        SubagentRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        SubagentRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}
