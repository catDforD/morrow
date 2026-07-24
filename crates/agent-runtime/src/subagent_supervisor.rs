use crate::subagent_store::{SubagentInstanceDocument, SubagentSessionStore};
use crate::{
    AgentEventEnvelope, EVENT_SCHEMA_VERSION, RuntimeError, maybe_auto_compact_with_tools,
    timestamp_ms,
};
use agent_config::{ContextConfig, ModelContextLimits};
use agent_core::{Agent, Model, ToolExecutionContext};
use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalOrigin, ApprovalRequest,
    FileChangeSummary, MAX_SUBAGENT_PROMPT_SUFFIX_CHARS, MAX_SUBAGENT_TIMEOUT_SECS,
    MAX_SUBAGENT_TOOL_ROUNDS, MIN_SUBAGENT_TIMEOUT_SECS, MIN_SUBAGENT_TOOL_ROUNDS, ModelInvocation,
    PermissionProfile, Session, ShellCommandSummary, SubagentIdentity, SubagentInstanceSnapshot,
    SubagentInstanceStatus, SubagentRole, SubagentRoleOverride, SubagentRunRecord,
    SubagentRunStatus, SubagentRunSummary, ToolExecutionSummary, TurnRecord, TurnStatus,
    TurnStepKind, default_subagent_identities,
};
use agent_tools::{
    BuiltInToolAllowlist, CancellationToken, MAX_SUBAGENT_TASK_CHARS, SubagentController,
    ToolRegistry, effective_subagent_permissions,
};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

pub const MAX_PERSISTENT_SUBAGENTS_PER_SESSION: usize = 8;
pub const MAX_CONCURRENT_SUBAGENT_RUNS: usize = 4;
pub const MAX_SUBAGENT_RESULT_CHARS: usize = 12_000;
const SUBAGENT_CANCELLATION_GRACE: Duration = Duration::from_secs(2);

static INSTANCE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait SubagentObserver: Send + Sync {
    fn on_event(&self, _event: &AgentEventEnvelope) {}

    fn resolve_approval(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'static, Result<ApprovalDecision, String>> {
        async move { Ok(ApprovalDecision::deny(request.id)) }.boxed()
    }

    fn cancel_approvals(
        &self,
        _instance_id: String,
        _run_id: Option<String>,
    ) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }
}

#[derive(Debug, Default)]
pub struct DenySubagentObserver;

impl SubagentObserver for DenySubagentObserver {}

#[derive(Clone)]
pub struct SubagentRoleRuntime {
    pub model: Arc<dyn Model>,
    pub invocation: ModelInvocation,
    pub limits: ModelContextLimits,
    pub role_config: SubagentRoleOverride,
    pub base_system_prompt: Arc<str>,
    pub parent_permissions: PermissionProfile,
}

impl std::fmt::Debug for SubagentRoleRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentRoleRuntime")
            .field("invocation", &self.invocation)
            .field("limits", &self.limits)
            .field("role_config", &self.role_config)
            .field("parent_permissions", &self.parent_permissions)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SubagentSupervisor {
    inner: Arc<SubagentSupervisorInner>,
}

struct SubagentSupervisorInner {
    workspace_root: PathBuf,
    session_name: String,
    context_config: ContextConfig,
    store: SubagentSessionStore,
    observer: Arc<dyn SubagentObserver>,
    roles: RwLock<BTreeMap<SubagentRole, SubagentRoleRuntime>>,
    models: RwLock<HashMap<String, AvailableModelRuntime>>,
    identities: RwLock<Vec<SubagentIdentity>>,
    state: Mutex<SupervisorState>,
    run_slots: Arc<Semaphore>,
    writer_slot: Arc<Semaphore>,
    changed: Notify,
    event_index: AtomicU64,
}

struct SupervisorState {
    instances: HashMap<String, ManagedInstance>,
}

struct ManagedInstance {
    document: SubagentInstanceDocument,
    cancellation: Option<CancellationToken>,
}

struct ChildTurnOutcome {
    session: Session,
    record: TurnRecord,
    summary: SubagentRunSummary,
}

struct ChildTurnRequest {
    document: SubagentInstanceDocument,
    runtime: SubagentRoleRuntime,
    instance_id: String,
    run_id: String,
    task: String,
    turn_index: usize,
    cancellation: CancellationToken,
}

struct SubagentSupervisorInit {
    workspace_root: PathBuf,
    session_name: String,
    context_config: ContextConfig,
    store: SubagentSessionStore,
    roles: BTreeMap<SubagentRole, SubagentRoleRuntime>,
    identities: Vec<SubagentIdentity>,
    observer: Arc<dyn SubagentObserver>,
    writer_slot: Arc<Semaphore>,
}

#[derive(Clone)]
struct AvailableModelRuntime {
    model: Arc<dyn Model>,
    invocation: ModelInvocation,
    limits: ModelContextLimits,
}

impl SubagentSupervisor {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        session_name: impl Into<String>,
        context_config: ContextConfig,
        roles: BTreeMap<SubagentRole, SubagentRoleRuntime>,
        identities: Vec<SubagentIdentity>,
        observer: Arc<dyn SubagentObserver>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_writer_lease(
            workspace_root,
            session_name,
            context_config,
            roles,
            identities,
            observer,
            Arc::new(Semaphore::new(1)),
        )
    }

    pub fn new_with_writer_lease(
        workspace_root: impl Into<PathBuf>,
        session_name: impl Into<String>,
        context_config: ContextConfig,
        roles: BTreeMap<SubagentRole, SubagentRoleRuntime>,
        identities: Vec<SubagentIdentity>,
        observer: Arc<dyn SubagentObserver>,
        writer_slot: Arc<Semaphore>,
    ) -> Result<Self, RuntimeError> {
        let workspace_root = workspace_root.into();
        let session_name = session_name.into();
        let store = SubagentSessionStore::for_workspace(&workspace_root, &session_name)?;
        Self::from_init(SubagentSupervisorInit {
            workspace_root,
            session_name,
            context_config,
            store,
            roles,
            identities,
            observer,
            writer_slot,
        })
    }

    fn from_init(init: SubagentSupervisorInit) -> Result<Self, RuntimeError> {
        let instances = init
            .store
            .load_recovered()?
            .into_iter()
            .map(|document| {
                (
                    document.snapshot.id.clone(),
                    ManagedInstance {
                        document,
                        cancellation: None,
                    },
                )
            })
            .collect();
        let models = init
            .roles
            .values()
            .map(|runtime| {
                (
                    model_key(&runtime.invocation),
                    AvailableModelRuntime {
                        model: runtime.model.clone(),
                        invocation: runtime.invocation.clone(),
                        limits: runtime.limits,
                    },
                )
            })
            .collect();
        Ok(Self {
            inner: Arc::new(SubagentSupervisorInner {
                workspace_root: init.workspace_root,
                session_name: init.session_name,
                context_config: init.context_config,
                store: init.store,
                observer: init.observer,
                roles: RwLock::new(init.roles),
                models: RwLock::new(models),
                identities: RwLock::new(if init.identities.is_empty() {
                    default_subagent_identities()
                } else {
                    init.identities
                }),
                state: Mutex::new(SupervisorState { instances }),
                run_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENT_RUNS)),
                writer_slot: init.writer_slot,
                changed: Notify::new(),
                event_index: AtomicU64::new(0),
            }),
        })
    }

    pub async fn update_runtime(
        &self,
        roles: BTreeMap<SubagentRole, SubagentRoleRuntime>,
        identities: Vec<SubagentIdentity>,
    ) {
        {
            let mut models = self.inner.models.write().await;
            for runtime in roles.values() {
                models.insert(
                    model_key(&runtime.invocation),
                    AvailableModelRuntime {
                        model: runtime.model.clone(),
                        invocation: runtime.invocation.clone(),
                        limits: runtime.limits,
                    },
                );
            }
        }
        *self.inner.roles.write().await = roles;
        if !identities.is_empty() {
            *self.inner.identities.write().await = identities;
        }
    }

    pub async fn register_model_runtime(
        &self,
        model: Arc<dyn Model>,
        invocation: ModelInvocation,
        limits: ModelContextLimits,
    ) {
        self.inner.models.write().await.insert(
            model_key(&invocation),
            AvailableModelRuntime {
                model,
                invocation,
                limits,
            },
        );
    }

    pub async fn required_models(&self) -> Vec<ModelInvocation> {
        let state = self.inner.state.lock().await;
        let mut seen = HashSet::new();
        state
            .instances
            .values()
            .filter_map(|instance| {
                let model = instance.document.model.clone();
                seen.insert(model_key(&model)).then_some(model)
            })
            .collect()
    }

    pub async fn snapshots(&self) -> Vec<SubagentInstanceSnapshot> {
        let state = self.inner.state.lock().await;
        let mut snapshots = state
            .instances
            .values()
            .map(|instance| instance.document.snapshot.clone())
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.created_at_ms);
        snapshots
    }

    pub async fn document(&self, instance_id: &str) -> Result<SubagentInstanceDocument, String> {
        let state = self.inner.state.lock().await;
        state
            .instances
            .get(instance_id)
            .map(|instance| instance.document.clone())
            .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))
    }

    pub fn events(&self, instance_id: &str) -> Result<Vec<AgentEventEnvelope>, RuntimeError> {
        Ok(self.inner.store.events(instance_id)?)
    }

    pub async fn has_active_runs(&self) -> bool {
        self.active_run_count().await > 0
    }

    pub async fn active_run_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .await
            .instances
            .values()
            .filter(|instance| instance.cancellation.is_some())
            .count()
    }

    pub async fn delete(&self, instance_id: &str) -> Result<(), String> {
        let mut state = self.inner.state.lock().await;
        let instance = state
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))?;
        if instance.cancellation.is_some() {
            return Err("active subagent instances must be cancelled before deletion".to_string());
        }
        self.inner
            .store
            .delete(instance_id)
            .map_err(|error| error.to_string())?;
        state.instances.remove(instance_id);
        drop(state);
        self.inner.changed.notify_waiters();
        Ok(())
    }

    async fn spawn_instance(
        &self,
        role: SubagentRole,
        task: String,
    ) -> Result<SubagentInstanceSnapshot, String> {
        let task = validate_subagent_input(task, "task")?;
        let runtime = self.runtime_for(role).await?;
        let identity = self.allocate_identity().await;
        let instance_id = next_instance_id();
        let run_id = next_run_id();
        let now = timestamp_ms();
        let permissions = effective_subagent_permissions(runtime.parent_permissions, role);
        let queue_reason = if role == SubagentRole::Worker {
            "waiting for writer lease"
        } else {
            "waiting for execution slot"
        };
        let system_prompt = effective_role_prompt(
            runtime.base_system_prompt.as_ref(),
            role,
            &runtime.role_config.prompt_suffix,
        );
        let snapshot = SubagentInstanceSnapshot {
            id: instance_id.clone(),
            role,
            identity,
            status: SubagentInstanceStatus::Queued,
            created_at_ms: now,
            updated_at_ms: now,
            latest_run_id: Some(run_id.clone()),
            latest_task: Some(task.clone()),
            queue_reason: Some(queue_reason.to_string()),
            latest_summary: None,
            event_log_truncated: false,
        };
        let mut document = SubagentInstanceDocument::new(
            snapshot.clone(),
            runtime.role_config.clone(),
            permissions,
            runtime.invocation.clone(),
            system_prompt,
        );
        document.runs.push(SubagentRunRecord {
            id: run_id.clone(),
            task: task.clone(),
            status: SubagentRunStatus::Queued,
            turn_index: 0,
            started_at_ms: now,
            completed_at_ms: None,
            summary: None,
        });
        let cancellation = CancellationToken::new();
        {
            let mut state = self.inner.state.lock().await;
            if state.instances.len() >= MAX_PERSISTENT_SUBAGENTS_PER_SESSION {
                return Err(format!(
                    "subagent instance limit exceeded ({MAX_PERSISTENT_SUBAGENTS_PER_SESSION} per session)"
                ));
            }
            self.inner
                .store
                .save(&document)
                .map_err(|error| error.to_string())?;
            state.instances.insert(
                instance_id.clone(),
                ManagedInstance {
                    document,
                    cancellation: Some(cancellation.clone()),
                },
            );
        }
        self.emit_snapshot(snapshot.clone()).await;
        self.spawn_run_task(instance_id, run_id, task, cancellation, runtime);
        Ok(snapshot)
    }

    async fn send_message(
        &self,
        instance_id: String,
        message: String,
    ) -> Result<SubagentInstanceSnapshot, String> {
        let message = validate_subagent_input(message, "message")?;
        let (model, role, role_config, permission_ceiling, system_prompt) = {
            let state = self.inner.state.lock().await;
            let instance = state
                .instances
                .get(&instance_id)
                .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))?;
            if instance.cancellation.is_some() {
                return Err(format!(
                    "subagent instance {instance_id:?} is already active"
                ));
            }
            (
                instance.document.model.clone(),
                instance.document.snapshot.role,
                instance.document.role_config.clone(),
                instance.document.permission_ceiling,
                instance.document.system_prompt.clone(),
            )
        };
        validate_role_config(role, &role_config)?;
        let available = self
            .inner
            .models
            .read()
            .await
            .get(&model_key(&model))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "model {} / {} required by subagent {instance_id:?} is unavailable; provide its model credentials before continuing",
                    model.provider_name, model.model_name
                )
            })?;
        let runtime = SubagentRoleRuntime {
            model: available.model,
            invocation: available.invocation,
            limits: available.limits,
            role_config,
            base_system_prompt: Arc::from(system_prompt),
            parent_permissions: permission_ceiling,
        };
        let run_id = next_run_id();
        let now = timestamp_ms();
        let cancellation = CancellationToken::new();
        let snapshot = {
            let mut state = self.inner.state.lock().await;
            let instance = state
                .instances
                .get_mut(&instance_id)
                .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))?;
            if instance.cancellation.is_some() {
                return Err(format!(
                    "subagent instance {instance_id:?} is already active"
                ));
            }
            let turn_index = instance.document.session.turns.len();
            instance.document.snapshot.status = SubagentInstanceStatus::Queued;
            instance.document.snapshot.updated_at_ms = now;
            instance.document.snapshot.latest_run_id = Some(run_id.clone());
            instance.document.snapshot.latest_task = Some(message.clone());
            instance.document.snapshot.queue_reason = Some(
                if instance.document.snapshot.role == SubagentRole::Worker {
                    "waiting for writer lease"
                } else {
                    "waiting for execution slot"
                }
                .to_string(),
            );
            instance.document.runs.push(SubagentRunRecord {
                id: run_id.clone(),
                task: message.clone(),
                status: SubagentRunStatus::Queued,
                turn_index,
                started_at_ms: now,
                completed_at_ms: None,
                summary: None,
            });
            instance.cancellation = Some(cancellation.clone());
            self.inner
                .store
                .save(&instance.document)
                .map_err(|error| error.to_string())?;
            instance.document.snapshot.clone()
        };
        self.emit_snapshot(snapshot.clone()).await;
        self.spawn_run_task(instance_id, run_id, message, cancellation, runtime);
        Ok(snapshot)
    }

    fn spawn_run_task(
        &self,
        instance_id: String,
        run_id: String,
        task: String,
        cancellation: CancellationToken,
        runtime: SubagentRoleRuntime,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            supervisor
                .run_instance(instance_id, run_id, task, cancellation, runtime)
                .await;
        });
    }

    async fn run_instance(
        &self,
        instance_id: String,
        run_id: String,
        task: String,
        cancellation: CancellationToken,
        runtime: SubagentRoleRuntime,
    ) {
        let role = match self.document(&instance_id).await {
            Ok(document) => document.snapshot.role,
            Err(_) => return,
        };
        let writer_permit = if role == SubagentRole::Worker {
            match acquire_permit(self.inner.writer_slot.clone(), &cancellation).await {
                Some(permit) => Some(permit),
                None => {
                    self.finish_without_turn(
                        &instance_id,
                        &run_id,
                        SubagentRunStatus::Cancelled,
                        "subagent run cancelled while waiting for writer lease",
                    )
                    .await;
                    return;
                }
            }
        } else {
            None
        };
        if writer_permit.is_some()
            && let Ok(snapshot) = self
                .set_status(
                    &instance_id,
                    &run_id,
                    SubagentInstanceStatus::Queued,
                    SubagentRunStatus::Queued,
                    Some("waiting for execution slot".to_string()),
                )
                .await
        {
            self.emit_snapshot(snapshot).await;
        }
        let run_permit = match acquire_permit(self.inner.run_slots.clone(), &cancellation).await {
            Some(permit) => permit,
            None => {
                drop(writer_permit);
                self.finish_without_turn(
                    &instance_id,
                    &run_id,
                    SubagentRunStatus::Cancelled,
                    "subagent run cancelled while waiting for an execution slot",
                )
                .await;
                return;
            }
        };
        if cancellation.is_cancelled() {
            drop(run_permit);
            drop(writer_permit);
            self.finish_without_turn(
                &instance_id,
                &run_id,
                SubagentRunStatus::Cancelled,
                "subagent run cancelled",
            )
            .await;
            return;
        }
        let snapshot = match self
            .set_status(
                &instance_id,
                &run_id,
                SubagentInstanceStatus::Running,
                SubagentRunStatus::Running,
                None,
            )
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => return,
        };
        self.emit_snapshot(snapshot).await;

        let document = match self.document(&instance_id).await {
            Ok(document) => document,
            Err(_) => return,
        };
        let turn_index = document.session.turns.len();
        let child_cancellation = CancellationToken::new();
        let execution = self.execute_child_turn(ChildTurnRequest {
            document,
            runtime: runtime.clone(),
            instance_id: instance_id.clone(),
            run_id: run_id.clone(),
            task: task.clone(),
            turn_index,
            cancellation: child_cancellation.clone(),
        });
        tokio::pin!(execution);
        let timeout = Duration::from_secs(runtime.role_config.timeout_secs);
        let (outcome, termination) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                child_cancellation.cancel();
                self.inner
                    .observer
                    .cancel_approvals(instance_id.clone(), Some(run_id.clone()))
                    .await;
                let outcome = match tokio::time::timeout(
                    SUBAGENT_CANCELLATION_GRACE,
                    &mut execution,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => Err(RuntimeError::AgentRun(
                        "subagent execution did not stop within the cancellation grace period"
                            .to_string(),
                    )),
                };
                (outcome, RunTermination::Cancelled)
            }
            _ = tokio::time::sleep(timeout) => {
                child_cancellation.cancel();
                self.inner
                    .observer
                    .cancel_approvals(instance_id.clone(), Some(run_id.clone()))
                    .await;
                let outcome = match tokio::time::timeout(
                    SUBAGENT_CANCELLATION_GRACE,
                    &mut execution,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => Err(RuntimeError::AgentRun(
                        "subagent execution did not stop within the timeout grace period"
                            .to_string(),
                    )),
                };
                (outcome, RunTermination::TimedOut)
            }
            outcome = &mut execution => (outcome, RunTermination::Natural),
        };
        drop(run_permit);
        drop(writer_permit);

        match outcome {
            Ok(mut outcome) => {
                match termination {
                    RunTermination::Cancelled => {
                        outcome.summary.status = SubagentRunStatus::Cancelled;
                        outcome.summary.error = Some("subagent run cancelled".to_string());
                        outcome.record.turn.status = TurnStatus::Failed;
                        outcome.record.turn.error = Some("subagent run cancelled".to_string());
                    }
                    RunTermination::TimedOut => {
                        let message = format!(
                            "subagent timed out after {} seconds",
                            runtime.role_config.timeout_secs
                        );
                        outcome.summary.status = SubagentRunStatus::Failed;
                        outcome.summary.error = Some(message.clone());
                        outcome.record.turn.status = TurnStatus::Failed;
                        outcome.record.turn.error = Some(message);
                    }
                    RunTermination::Natural => {}
                }
                self.commit_outcome(&instance_id, &run_id, outcome).await;
            }
            Err(error) => {
                let (status, message) = match termination {
                    RunTermination::Cancelled => (
                        SubagentRunStatus::Cancelled,
                        "subagent run cancelled".to_string(),
                    ),
                    RunTermination::TimedOut => (
                        SubagentRunStatus::Failed,
                        format!(
                            "subagent timed out after {} seconds",
                            runtime.role_config.timeout_secs
                        ),
                    ),
                    RunTermination::Natural => (SubagentRunStatus::Failed, error.to_string()),
                };
                self.finish_without_turn(&instance_id, &run_id, status, message)
                    .await;
            }
        }
    }

    async fn execute_child_turn(
        &self,
        request: ChildTurnRequest,
    ) -> Result<ChildTurnOutcome, RuntimeError> {
        let ChildTurnRequest {
            mut document,
            runtime,
            instance_id,
            run_id,
            task,
            turn_index,
            cancellation,
        } = request;
        let allowed =
            BuiltInToolAllowlist::for_subagent(document.snapshot.role, document.permission_ceiling);
        let writer_lease = (document.snapshot.role == SubagentRole::Reviewer)
            .then(|| self.inner.writer_slot.clone());
        let tools = ToolRegistry::built_in_with_allowlist_and_writer_lease(
            &self.inner.workspace_root,
            document.permission_ceiling,
            allowed,
            writer_lease,
        )?;
        if let Err(error) = maybe_auto_compact_with_tools(
            runtime.model.as_ref(),
            &document.system_prompt,
            &mut document.session,
            self.inner.context_config,
            runtime.limits,
            &task,
            &tools.definitions(),
        )
        .await
        {
            let record = failed_record(&task, &document.model, error.to_string());
            let summary = failed_summary(
                &instance_id,
                &run_id,
                document.snapshot.role,
                &task,
                error.to_string(),
                timestamp_ms(),
            );
            return Ok(ChildTurnOutcome {
                session: document.session,
                record,
                summary,
            });
        }

        let agent = Agent::with_tools(runtime.model.as_ref(), &document.system_prompt, &tools)
            .with_max_tool_rounds(document.role_config.max_tool_rounds);
        let mut stream = agent
            .run_turn_with_context(
                &document.session.active_thread,
                task.clone(),
                ToolExecutionContext {
                    cancellation: cancellation.clone(),
                },
            )
            .await?;
        let started_at_ms = timestamp_ms();
        let mut event_index = 0usize;
        let mut cancellation_observed = false;
        let mut file_changes = Vec::new();
        let mut shell_commands = Vec::new();
        let mut active_external_approval: Option<(String, String)> = None;

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
            let Some(mut event) = event else {
                break;
            };

            if let AgentEvent::ToolCallFinished {
                summary: Some(summary),
                ..
            } = &event
            {
                collect_execution_summary(summary, &mut file_changes, &mut shell_commands);
            }

            if let AgentEvent::ApprovalRequested(internal_request) = event {
                let internal_id = internal_request.id.clone();
                let external_id = format!("approval-{run_id}-{internal_id}");
                let external_request = ApprovalRequest {
                    id: external_id.clone(),
                    action: internal_request.action.clone(),
                    reason: internal_request.reason.clone(),
                    origin: ApprovalOrigin::SubagentRun {
                        instance_id: instance_id.clone(),
                        run_id: run_id.clone(),
                        role: document.snapshot.role,
                        identity_id: Some(document.snapshot.identity.id.clone()),
                        identity_name: Some(document.snapshot.identity.name.clone()),
                        tool_call_id: internal_id.strip_prefix("approval-").map(str::to_string),
                    },
                };
                event = AgentEvent::ApprovalRequested(external_request.clone());
                self.emit_child_event(
                    &instance_id,
                    &run_id,
                    &document.snapshot,
                    turn_index,
                    event_index,
                    event,
                )
                .await;
                event_index += 1;
                if let Ok(snapshot) = self
                    .set_status(
                        &instance_id,
                        &run_id,
                        SubagentInstanceStatus::WaitingApproval,
                        SubagentRunStatus::WaitingApproval,
                        None,
                    )
                    .await
                {
                    self.emit_snapshot(snapshot).await;
                }
                active_external_approval = Some((internal_id.clone(), external_id));
                let decision = if cancellation.is_cancelled() {
                    ApprovalDecision::deny(internal_id)
                } else {
                    match self.inner.observer.resolve_approval(external_request).await {
                        Ok(decision) if decision.approved => ApprovalDecision::approve(internal_id),
                        _ => ApprovalDecision::deny(internal_id),
                    }
                };
                stream.resolve_approval(decision)?;
                if !cancellation.is_cancelled()
                    && let Ok(snapshot) = self
                        .set_status(
                            &instance_id,
                            &run_id,
                            SubagentInstanceStatus::Running,
                            SubagentRunStatus::Running,
                            None,
                        )
                        .await
                {
                    self.emit_snapshot(snapshot).await;
                }
                continue;
            }

            if let AgentEvent::ApprovalResolved(decision) = &mut event
                && let Some((internal_id, external_id)) = active_external_approval.as_ref()
                && decision.request_id == *internal_id
            {
                decision.request_id = external_id.clone();
                active_external_approval = None;
            }

            self.emit_child_event(
                &instance_id,
                &run_id,
                &document.snapshot,
                turn_index,
                event_index,
                event,
            )
            .await;
            event_index += 1;
        }

        let mut record = stream.into_turn_record();
        if record.turn.model.is_none() {
            record.turn.model = Some(document.model.clone());
        }
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
        let completed_at_ms = timestamp_ms();
        let (result, truncated) = record
            .turn
            .assistant_message
            .as_ref()
            .and_then(|message| message.content.clone())
            .map(|value| truncate_chars(value, MAX_SUBAGENT_RESULT_CHARS))
            .unwrap_or((String::new(), false));
        let status = if record.turn.status == TurnStatus::Completed {
            SubagentRunStatus::Completed
        } else if cancellation.is_cancelled() {
            SubagentRunStatus::Cancelled
        } else {
            SubagentRunStatus::Failed
        };
        let error = if status == SubagentRunStatus::Completed {
            None
        } else {
            Some(
                record
                    .turn
                    .error
                    .clone()
                    .unwrap_or_else(|| "subagent run failed".to_string()),
            )
        };
        let summary = SubagentRunSummary {
            instance_id,
            run_id,
            role: document.snapshot.role,
            status,
            task,
            result: (!result.trim().is_empty()).then_some(result),
            error,
            model_calls,
            tool_calls,
            file_changes,
            shell_commands,
            started_at_ms,
            completed_at_ms: Some(completed_at_ms),
            truncated,
        };
        Ok(ChildTurnOutcome {
            session: document.session,
            record,
            summary,
        })
    }

    async fn commit_outcome(&self, instance_id: &str, run_id: &str, outcome: ChildTurnOutcome) {
        let snapshot = {
            let mut state = self.inner.state.lock().await;
            let Some(instance) = state.instances.get_mut(instance_id) else {
                return;
            };
            instance.document.session = outcome.session;
            if instance
                .document
                .session
                .try_apply_turn(outcome.record)
                .is_err()
            {
                return;
            }
            let summary = outcome.summary;
            if let Some(run) = instance
                .document
                .runs
                .iter_mut()
                .find(|run| run.id == run_id)
            {
                run.status = summary.status;
                run.completed_at_ms = summary.completed_at_ms;
                run.summary = Some(summary.clone());
            }
            instance.document.snapshot.status = instance_status_for_run(summary.status);
            instance.document.snapshot.updated_at_ms = timestamp_ms();
            instance.document.snapshot.queue_reason = None;
            instance.document.snapshot.latest_summary = Some(summary);
            instance.cancellation = None;
            if self.inner.store.save(&instance.document).is_err() {
                instance.document.snapshot.status = SubagentInstanceStatus::Failed;
            }
            instance.document.snapshot.clone()
        };
        self.inner.changed.notify_waiters();
        self.emit_snapshot(snapshot).await;
    }

    async fn finish_without_turn(
        &self,
        instance_id: &str,
        run_id: &str,
        status: SubagentRunStatus,
        error: impl Into<String>,
    ) {
        let error = error.into();
        let snapshot = {
            let mut state = self.inner.state.lock().await;
            let Some(instance) = state.instances.get_mut(instance_id) else {
                return;
            };
            let now = timestamp_ms();
            let (task, started_at_ms) = instance
                .document
                .runs
                .iter()
                .find(|run| run.id == run_id)
                .map(|run| (run.task.clone(), run.started_at_ms))
                .unwrap_or_else(|| (String::new(), now));
            let summary = SubagentRunSummary {
                instance_id: instance_id.to_string(),
                run_id: run_id.to_string(),
                role: instance.document.snapshot.role,
                status,
                task,
                result: None,
                error: Some(error),
                model_calls: 0,
                tool_calls: 0,
                file_changes: Vec::new(),
                shell_commands: Vec::new(),
                started_at_ms,
                completed_at_ms: Some(now),
                truncated: false,
            };
            if let Some(run) = instance
                .document
                .runs
                .iter_mut()
                .find(|run| run.id == run_id)
            {
                run.status = status;
                run.completed_at_ms = Some(now);
                run.summary = Some(summary.clone());
            }
            instance.document.snapshot.status = instance_status_for_run(status);
            instance.document.snapshot.updated_at_ms = now;
            instance.document.snapshot.queue_reason = None;
            instance.document.snapshot.latest_summary = Some(summary);
            instance.cancellation = None;
            let _ = self.inner.store.save(&instance.document);
            instance.document.snapshot.clone()
        };
        self.inner.changed.notify_waiters();
        self.emit_snapshot(snapshot).await;
    }

    async fn set_status(
        &self,
        instance_id: &str,
        run_id: &str,
        instance_status: SubagentInstanceStatus,
        run_status: SubagentRunStatus,
        queue_reason: Option<String>,
    ) -> Result<SubagentInstanceSnapshot, String> {
        let mut state = self.inner.state.lock().await;
        let instance = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))?;
        instance.document.snapshot.status = instance_status;
        instance.document.snapshot.updated_at_ms = timestamp_ms();
        instance.document.snapshot.queue_reason = queue_reason;
        if let Some(run) = instance
            .document
            .runs
            .iter_mut()
            .find(|run| run.id == run_id)
        {
            run.status = run_status;
        }
        self.inner
            .store
            .save(&instance.document)
            .map_err(|error| error.to_string())?;
        self.inner.changed.notify_waiters();
        Ok(instance.document.snapshot.clone())
    }

    async fn runtime_for(&self, role: SubagentRole) -> Result<SubagentRoleRuntime, String> {
        let runtime = self
            .inner
            .roles
            .read()
            .await
            .get(&role)
            .cloned()
            .ok_or_else(|| format!("subagent role {} is unavailable", role.as_str()))?;
        validate_role_config(role, &runtime.role_config)?;
        Ok(runtime)
    }

    async fn allocate_identity(&self) -> SubagentIdentity {
        let identities = self.inner.identities.read().await;
        let state = self.inner.state.lock().await;
        let used = state
            .instances
            .values()
            .map(|instance| instance.document.snapshot.identity.id.as_str())
            .collect::<HashSet<_>>();
        identities
            .iter()
            .find(|identity| !used.contains(identity.id.as_str()))
            .cloned()
            .unwrap_or_else(|| {
                let index = INSTANCE_ID_COUNTER.load(Ordering::Relaxed) as usize % identities.len();
                identities[index].clone()
            })
    }

    async fn inspect_instances(
        &self,
        instance_id: Option<String>,
    ) -> Result<Vec<SubagentInstanceSnapshot>, String> {
        let snapshots = self.snapshots().await;
        match instance_id {
            Some(id) => snapshots
                .into_iter()
                .find(|snapshot| snapshot.id == id)
                .map(|snapshot| vec![snapshot])
                .ok_or_else(|| format!("subagent instance {id:?} was not found")),
            None => Ok(snapshots),
        }
    }

    async fn wait_instances(
        &self,
        ids: Vec<String>,
        timeout: Duration,
    ) -> Result<Vec<SubagentInstanceSnapshot>, String> {
        let wait = async {
            loop {
                let notified = self.inner.changed.notified();
                let (selected, active) = {
                    let state = self.inner.state.lock().await;
                    let selected = ids
                        .iter()
                        .map(|id| {
                            state
                                .instances
                                .get(id)
                                .map(|instance| instance.document.snapshot.clone())
                                .ok_or_else(|| format!("subagent instance {id:?} was not found"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let active = ids.iter().any(|id| {
                        state
                            .instances
                            .get(id)
                            .is_some_and(|instance| instance.cancellation.is_some())
                    });
                    (selected, active)
                };
                if !active {
                    return Ok(selected);
                }
                notified.await;
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => {
                let instances = self.inspect_instances(None).await?;
                ids.iter()
                    .map(|id| {
                        instances
                            .iter()
                            .find(|instance| instance.id == *id)
                            .cloned()
                            .ok_or_else(|| format!("subagent instance {id:?} was not found"))
                    })
                    .collect()
            }
        }
    }

    async fn cancel_instance(
        &self,
        instance_id: String,
    ) -> Result<SubagentInstanceSnapshot, String> {
        let (snapshot, run_id) = {
            let mut state = self.inner.state.lock().await;
            let instance = state
                .instances
                .get_mut(&instance_id)
                .ok_or_else(|| format!("subagent instance {instance_id:?} was not found"))?;
            if let Some(cancellation) = instance.cancellation.as_ref() {
                cancellation.cancel();
            }
            let run_id = instance.document.snapshot.latest_run_id.clone();
            if instance.document.snapshot.status.is_active() {
                instance.document.snapshot.status = SubagentInstanceStatus::Cancelled;
                instance.document.snapshot.updated_at_ms = timestamp_ms();
                instance.document.snapshot.queue_reason = None;
                self.inner
                    .store
                    .save(&instance.document)
                    .map_err(|error| error.to_string())?;
            }
            (instance.document.snapshot.clone(), run_id)
        };
        self.inner
            .observer
            .cancel_approvals(instance_id, run_id)
            .await;
        self.inner.changed.notify_waiters();
        self.emit_snapshot(snapshot.clone()).await;
        Ok(snapshot)
    }

    async fn emit_snapshot(&self, snapshot: SubagentInstanceSnapshot) {
        let event_index = self.inner.event_index.fetch_add(1, Ordering::Relaxed) as usize;
        self.inner.observer.on_event(&AgentEventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            timestamp_ms: timestamp_ms(),
            session: self.inner.session_name.clone(),
            workspace_root: self.inner.workspace_root.display().to_string(),
            origin: AgentEventOrigin::Session,
            turn_index: 0,
            event_index,
            event: AgentEvent::SubagentUpdated(Box::new(snapshot)),
        });
    }

    async fn emit_child_event(
        &self,
        instance_id: &str,
        run_id: &str,
        snapshot: &SubagentInstanceSnapshot,
        turn_index: usize,
        event_index: usize,
        event: AgentEvent,
    ) {
        let envelope = AgentEventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            timestamp_ms: timestamp_ms(),
            session: self.inner.session_name.clone(),
            workspace_root: self.inner.workspace_root.display().to_string(),
            origin: AgentEventOrigin::SubagentRun {
                instance_id: instance_id.to_string(),
                run_id: run_id.to_string(),
                role: snapshot.role,
                identity_id: Some(snapshot.identity.id.clone()),
                identity_name: Some(snapshot.identity.name.clone()),
                turn_index,
            },
            turn_index,
            event_index,
            event,
        };
        let persisted = self
            .inner
            .store
            .append_event(instance_id, &envelope)
            .unwrap_or(false);
        if !persisted {
            let mut state = self.inner.state.lock().await;
            if let Some(instance) = state.instances.get_mut(instance_id)
                && !instance.document.snapshot.event_log_truncated
            {
                instance.document.snapshot.event_log_truncated = true;
                let _ = self.inner.store.save(&instance.document);
            }
        }
        self.inner.observer.on_event(&envelope);
    }
}

impl SubagentController for SubagentSupervisor {
    fn writer_lease(&self) -> Option<Arc<Semaphore>> {
        Some(self.inner.writer_slot.clone())
    }

    fn spawn(
        &self,
        role: SubagentRole,
        task: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>> {
        let supervisor = self.clone();
        async move { supervisor.spawn_instance(role, task).await }.boxed()
    }

    fn send(
        &self,
        instance_id: String,
        message: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>> {
        let supervisor = self.clone();
        async move { supervisor.send_message(instance_id, message).await }.boxed()
    }

    fn inspect(
        &self,
        instance_id: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<SubagentInstanceSnapshot>, String>> {
        let supervisor = self.clone();
        async move { supervisor.inspect_instances(instance_id).await }.boxed()
    }

    fn wait(
        &self,
        instance_ids: Vec<String>,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<Vec<SubagentInstanceSnapshot>, String>> {
        let supervisor = self.clone();
        async move { supervisor.wait_instances(instance_ids, timeout).await }.boxed()
    }

    fn cancel(
        &self,
        instance_id: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>> {
        let supervisor = self.clone();
        async move { supervisor.cancel_instance(instance_id).await }.boxed()
    }
}

async fn acquire_permit(
    semaphore: Arc<Semaphore>,
    cancellation: &CancellationToken,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        permit = semaphore.acquire_owned() => permit.ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunTermination {
    Natural,
    Cancelled,
    TimedOut,
}

fn collect_execution_summary(
    summary: &ToolExecutionSummary,
    file_changes: &mut Vec<FileChangeSummary>,
    shell_commands: &mut Vec<ShellCommandSummary>,
) {
    file_changes.extend(summary.files.iter().cloned());
    if let Some(shell) = summary.shell.as_ref() {
        shell_commands.push(shell.clone());
    }
}

fn instance_status_for_run(status: SubagentRunStatus) -> SubagentInstanceStatus {
    match status {
        SubagentRunStatus::Completed => SubagentInstanceStatus::Idle,
        SubagentRunStatus::Failed => SubagentInstanceStatus::Failed,
        SubagentRunStatus::Cancelled => SubagentInstanceStatus::Cancelled,
        SubagentRunStatus::Interrupted => SubagentInstanceStatus::Interrupted,
        SubagentRunStatus::Queued => SubagentInstanceStatus::Queued,
        SubagentRunStatus::Running => SubagentInstanceStatus::Running,
        SubagentRunStatus::WaitingApproval => SubagentInstanceStatus::WaitingApproval,
    }
}

fn effective_role_prompt(base: &str, role: SubagentRole, suffix: &str) -> String {
    let guidance = match role {
        SubagentRole::Explore => {
            "You are an explore subagent. Investigate the workspace without modifying it. Return concise evidence with file paths, symbols, conclusions, and unresolved uncertainty."
        }
        SubagentRole::Plan => {
            "You are a planning subagent. Inspect the workspace without modifying it and return a decision-complete implementation plan, including interfaces, edge cases, and verification."
        }
        SubagentRole::Worker => {
            "You are a worker subagent. Make only the requested focused workspace changes, respect every approval boundary, verify the result, and summarize files changed, commands run, failures, and remaining risks."
        }
        SubagentRole::Reviewer => {
            "You are a reviewer subagent. Do not edit files. Review the requested code or design, prioritize concrete findings, cite file paths and symbols, and use shell commands only after explicit approval."
        }
    };
    if suffix.trim().is_empty() {
        format!("{base}\n\n{guidance}\n\nDo not delegate to other agents or use MCP tools.")
    } else {
        format!(
            "{base}\n\n{guidance}\n\nDo not delegate to other agents or use MCP tools.\n\nAdditional role instructions:\n{}",
            suffix.trim()
        )
    }
}

fn failed_record(task: &str, model: &ModelInvocation, error: String) -> TurnRecord {
    let mut record = TurnRecord::failed_user_prompt(task, error);
    record.turn.model = Some(model.clone());
    record
}

fn failed_summary(
    instance_id: &str,
    run_id: &str,
    role: SubagentRole,
    task: &str,
    error: String,
    now: u64,
) -> SubagentRunSummary {
    SubagentRunSummary {
        instance_id: instance_id.to_string(),
        run_id: run_id.to_string(),
        role,
        status: SubagentRunStatus::Failed,
        task: task.to_string(),
        result: None,
        error: Some(error),
        model_calls: 0,
        tool_calls: 0,
        file_changes: Vec::new(),
        shell_commands: Vec::new(),
        started_at_ms: now,
        completed_at_ms: Some(now),
        truncated: false,
    }
}

fn truncate_chars(value: String, max_chars: usize) -> (String, bool) {
    if value.chars().count() <= max_chars {
        return (value, false);
    }
    (value.chars().take(max_chars).collect(), true)
}

fn validate_subagent_input(value: String, label: &str) -> Result<String, String> {
    let value = value.trim().to_string();
    let chars = value.chars().count();
    if chars == 0 || chars > MAX_SUBAGENT_TASK_CHARS {
        return Err(format!(
            "subagent {label} must contain between 1 and {MAX_SUBAGENT_TASK_CHARS} characters"
        ));
    }
    Ok(value)
}

fn validate_role_config(role: SubagentRole, config: &SubagentRoleOverride) -> Result<(), String> {
    if config.prompt_suffix.chars().count() > MAX_SUBAGENT_PROMPT_SUFFIX_CHARS {
        return Err(format!(
            "{} prompt suffix must not exceed {MAX_SUBAGENT_PROMPT_SUFFIX_CHARS} characters",
            role.as_str()
        ));
    }
    if !(MIN_SUBAGENT_TIMEOUT_SECS..=MAX_SUBAGENT_TIMEOUT_SECS).contains(&config.timeout_secs) {
        return Err(format!(
            "{} timeout must be between {MIN_SUBAGENT_TIMEOUT_SECS} and {MAX_SUBAGENT_TIMEOUT_SECS} seconds",
            role.as_str()
        ));
    }
    if !(MIN_SUBAGENT_TOOL_ROUNDS..=MAX_SUBAGENT_TOOL_ROUNDS).contains(&config.max_tool_rounds) {
        return Err(format!(
            "{} max tool rounds must be between {MIN_SUBAGENT_TOOL_ROUNDS} and {MAX_SUBAGENT_TOOL_ROUNDS}",
            role.as_str()
        ));
    }
    Ok(())
}

fn next_instance_id() -> String {
    let counter = INSTANCE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("subagent-{:016x}-{counter:04x}", timestamp_ms())
}

fn next_run_id() -> String {
    let counter = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("subrun-{:016x}-{counter:04x}", timestamp_ms())
}

fn model_key(invocation: &ModelInvocation) -> String {
    format!(
        "{}\u{0}{}\u{0}{:?}",
        invocation.provider_id, invocation.model_id, invocation.reasoning
    )
}

pub fn build_role_runtimes(
    model: Arc<dyn Model>,
    invocation: ModelInvocation,
    limits: ModelContextLimits,
    base_system_prompt: impl Into<Arc<str>>,
    parent_permissions: PermissionProfile,
    overrides: &BTreeMap<SubagentRole, SubagentRoleOverride>,
) -> BTreeMap<SubagentRole, SubagentRoleRuntime> {
    let base_system_prompt = base_system_prompt.into();
    SubagentRole::ALL
        .into_iter()
        .map(|role| {
            (
                role,
                SubagentRoleRuntime {
                    model: model.clone(),
                    invocation: invocation.clone(),
                    limits,
                    role_config: overrides.get(&role).cloned().unwrap_or_default(),
                    base_system_prompt: base_system_prompt.clone(),
                    parent_permissions,
                },
            )
        })
        .collect()
}

pub fn subagent_store_for_session(
    workspace: &Path,
    session_name: &str,
) -> Result<SubagentSessionStore, RuntimeError> {
    Ok(SubagentSessionStore::for_workspace(
        workspace,
        session_name,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ModelEvent, ModelFailure, ModelRequest};
    use agent_protocol::{PermissionMode, ReasoningLevel, ShellPolicy};
    use futures_util::stream::{self, StreamExt};
    use std::env;
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone)]
    struct ConstantModel {
        text: String,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl ConstantModel {
        fn new(text: impl Into<String>) -> Self {
            Self {
                text: text.into(),
                requests: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    impl Model for ConstantModel {
        fn stream(&self, request: ModelRequest) -> agent_core::ModelFuture {
            self.requests.lock().expect("record requests").push(request);
            let text = self.text.clone();
            async move {
                let stream: agent_core::ModelStream = stream::iter(vec![
                    Ok(ModelEvent::TextDelta(text)),
                    Ok(ModelEvent::Completed),
                ])
                .boxed();
                Ok(stream)
            }
            .boxed()
        }
    }

    #[derive(Clone, Default)]
    struct PendingStreamModel {
        started: Arc<AtomicU64>,
    }

    impl Model for PendingStreamModel {
        fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
            self.started.fetch_add(1, Ordering::AcqRel);
            async move {
                let stream: agent_core::ModelStream =
                    Box::pin(stream::pending::<Result<ModelEvent, ModelFailure>>());
                Ok(stream)
            }
            .boxed()
        }
    }

    fn unique_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "morrow-subagent-supervisor-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn invocation(model_id: impl Into<String>) -> ModelInvocation {
        let model_id = model_id.into();
        ModelInvocation {
            provider_id: "test-provider".to_string(),
            provider_name: "Test Provider".to_string(),
            model_id: model_id.clone(),
            model_name: model_id,
            reasoning: ReasoningLevel::Off,
        }
    }

    fn limits() -> ModelContextLimits {
        ModelContextLimits {
            context_window_tokens: 128_000,
            reserved_output_tokens: 4_000,
        }
    }

    fn role_runtime(
        role: SubagentRole,
        model: Arc<dyn Model>,
        generation: &str,
    ) -> SubagentRoleRuntime {
        SubagentRoleRuntime {
            model,
            invocation: invocation(format!("{}-{generation}", role.as_str())),
            limits: limits(),
            role_config: SubagentRoleOverride::default(),
            base_system_prompt: Arc::from("test system prompt"),
            parent_permissions: PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Prompt,
            },
        }
    }

    fn roles_with(
        generation: &str,
        mut model_for: impl FnMut(SubagentRole) -> Arc<dyn Model>,
    ) -> BTreeMap<SubagentRole, SubagentRoleRuntime> {
        SubagentRole::ALL
            .into_iter()
            .map(|role| (role, role_runtime(role, model_for(role), generation)))
            .collect()
    }

    fn test_supervisor(
        label: &str,
        roles: BTreeMap<SubagentRole, SubagentRoleRuntime>,
    ) -> (SubagentSupervisor, PathBuf, PathBuf) {
        let store_root = unique_dir(&format!("{label}-store"));
        let workspace = unique_dir(&format!("{label}-workspace"));
        let store = SubagentSessionStore::new(&store_root, &workspace, "default")
            .expect("create subagent store");
        let supervisor = SubagentSupervisor::from_init(SubagentSupervisorInit {
            workspace_root: workspace.clone(),
            session_name: "default".to_string(),
            context_config: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 6,
                summary_target_tokens: 12_000,
                compact_max_retries: 2,
            },
            store,
            roles,
            identities: default_subagent_identities(),
            observer: Arc::new(DenySubagentObserver),
            writer_slot: Arc::new(Semaphore::new(1)),
        })
        .expect("create supervisor");
        (supervisor, store_root, workspace)
    }

    async fn wait_for_count(counter: &AtomicU64, expected: u64) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while counter.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("counter reached expected value");
    }

    async fn wait_until_idle(supervisor: &SubagentSupervisor) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while supervisor.has_active_runs().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supervisor became idle");
    }

    fn cleanup(supervisor: SubagentSupervisor, store_root: PathBuf, workspace: PathBuf) {
        drop(supervisor);
        let _ = std::fs::remove_dir_all(store_root);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn instance_limit_and_input_budget_are_enforced_at_the_supervisor_boundary() {
        let model: Arc<dyn Model> = Arc::new(ConstantModel::new("done"));
        let roles = roles_with("capacity", |_| model.clone());
        let (supervisor, store_root, workspace) = test_supervisor("capacity", roles);

        assert!(
            supervisor
                .spawn_instance(SubagentRole::Explore, "   ".to_string())
                .await
                .is_err()
        );
        assert!(
            supervisor
                .spawn_instance(
                    SubagentRole::Explore,
                    "x".repeat(MAX_SUBAGENT_TASK_CHARS + 1),
                )
                .await
                .is_err()
        );

        let mut ids = Vec::new();
        for index in 0..MAX_PERSISTENT_SUBAGENTS_PER_SESSION {
            ids.push(
                supervisor
                    .spawn_instance(SubagentRole::Explore, format!("task {index}"))
                    .await
                    .expect("spawn within capacity")
                    .id,
            );
        }
        assert!(
            supervisor
                .spawn_instance(SubagentRole::Explore, "overflow".to_string())
                .await
                .expect_err("ninth instance must fail")
                .contains("instance limit")
        );
        supervisor
            .wait_instances(ids, Duration::from_secs(2))
            .await
            .expect("runs finish");
        cleanup(supervisor, store_root, workspace);
    }

    #[tokio::test]
    async fn only_four_runs_execute_while_the_fifth_remains_queued() {
        let pending = PendingStreamModel::default();
        let model: Arc<dyn Model> = Arc::new(pending.clone());
        let roles = roles_with("parallel", |_| model.clone());
        let (supervisor, store_root, workspace) = test_supervisor("parallel", roles);
        let mut ids = Vec::new();
        for index in 0..5 {
            ids.push(
                supervisor
                    .spawn_instance(SubagentRole::Explore, format!("task {index}"))
                    .await
                    .expect("spawn run")
                    .id,
            );
        }

        wait_for_count(&pending.started, MAX_CONCURRENT_SUBAGENT_RUNS as u64).await;
        let snapshots = supervisor.snapshots().await;
        assert_eq!(
            snapshots
                .iter()
                .filter(|snapshot| snapshot.status == SubagentInstanceStatus::Running)
                .count(),
            MAX_CONCURRENT_SUBAGENT_RUNS
        );
        assert_eq!(
            snapshots
                .iter()
                .filter(|snapshot| snapshot.status == SubagentInstanceStatus::Queued)
                .count(),
            1
        );

        for id in ids {
            supervisor.cancel_instance(id).await.expect("cancel run");
        }
        wait_until_idle(&supervisor).await;
        cleanup(supervisor, store_root, workspace);
    }

    #[tokio::test]
    async fn worker_writer_lease_blocks_other_workers_but_not_read_only_runs_and_cancel_releases_it()
     {
        let workers = PendingStreamModel::default();
        let worker_model: Arc<dyn Model> = Arc::new(workers.clone());
        let read_model: Arc<dyn Model> = Arc::new(ConstantModel::new("read complete"));
        let mut roles = roles_with("writer", |role| {
            if role == SubagentRole::Worker {
                worker_model.clone()
            } else {
                read_model.clone()
            }
        });
        roles
            .get_mut(&SubagentRole::Worker)
            .expect("worker runtime")
            .parent_permissions = PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Prompt,
        };
        let (supervisor, store_root, workspace) = test_supervisor("writer", roles);
        let first = supervisor
            .spawn_instance(SubagentRole::Worker, "first writer".to_string())
            .await
            .expect("spawn first writer");
        wait_for_count(&workers.started, 1).await;
        let second = supervisor
            .spawn_instance(SubagentRole::Worker, "second writer".to_string())
            .await
            .expect("spawn second writer");
        let reader = supervisor
            .spawn_instance(SubagentRole::Explore, "read in parallel".to_string())
            .await
            .expect("spawn reader");

        let reader_snapshot = supervisor
            .wait_instances(vec![reader.id], Duration::from_secs(2))
            .await
            .expect("reader finishes")
            .pop()
            .expect("reader snapshot");
        assert_eq!(reader_snapshot.status, SubagentInstanceStatus::Idle);
        assert_eq!(workers.started.load(Ordering::Acquire), 1);
        assert_eq!(
            supervisor
                .document(&second.id)
                .await
                .expect("second writer document")
                .snapshot
                .queue_reason
                .as_deref(),
            Some("waiting for writer lease")
        );

        supervisor
            .cancel_instance(first.id)
            .await
            .expect("cancel first writer");
        wait_for_count(&workers.started, 2).await;
        supervisor
            .cancel_instance(second.id)
            .await
            .expect("cancel second writer");
        wait_until_idle(&supervisor).await;
        cleanup(supervisor, store_root, workspace);
    }

    #[tokio::test]
    async fn follow_up_keeps_context_and_uses_the_instance_model_snapshot() {
        let original = ConstantModel::new("original reply");
        let original_requests = original.requests.clone();
        let original_model: Arc<dyn Model> = Arc::new(original);
        let roles = roles_with("original", |_| original_model.clone());
        let (supervisor, store_root, workspace) = test_supervisor("follow-up", roles);
        let instance = supervisor
            .spawn_instance(SubagentRole::Explore, "first question".to_string())
            .await
            .expect("spawn instance");
        supervisor
            .wait_instances(vec![instance.id.clone()], Duration::from_secs(2))
            .await
            .expect("first run finishes");

        let replacement = ConstantModel::new("replacement reply");
        let replacement_requests = replacement.requests.clone();
        let replacement_model: Arc<dyn Model> = Arc::new(replacement);
        let mut replacement_roles = roles_with("replacement", |_| replacement_model.clone());
        for runtime in replacement_roles.values_mut() {
            runtime.base_system_prompt = Arc::from("replacement system prompt");
            runtime.role_config.prompt_suffix = "replacement suffix".to_string();
        }
        let mut replacement_identities = default_subagent_identities();
        replacement_identities[0].name = "replacement identity".to_string();
        supervisor
            .update_runtime(replacement_roles, replacement_identities)
            .await;
        supervisor
            .send_message(instance.id.clone(), "second question".to_string())
            .await
            .expect("continue instance");
        let follow_up = supervisor
            .wait_instances(vec![instance.id], Duration::from_secs(2))
            .await
            .expect("follow-up finishes")
            .pop()
            .expect("follow-up snapshot");
        assert_eq!(follow_up.identity.name, "后藤一里");

        let requests = original_requests.lock().expect("original requests");
        assert_eq!(requests.len(), 2);
        let second_messages = &requests[1].conversation.messages;
        assert!(
            second_messages
                .iter()
                .any(|message| { message.content.as_deref() == Some("first question") })
        );
        assert!(
            second_messages
                .iter()
                .any(|message| { message.content.as_deref() == Some("original reply") })
        );
        assert!(
            second_messages
                .iter()
                .any(|message| { message.content.as_deref() == Some("second question") })
        );
        let system_prompt = second_messages
            .iter()
            .find(|message| message.role == agent_protocol::Role::System)
            .and_then(|message| message.content.as_deref())
            .expect("system prompt");
        assert!(system_prompt.contains("test system prompt"));
        assert!(!system_prompt.contains("replacement system prompt"));
        assert!(!system_prompt.contains("replacement suffix"));
        assert!(
            replacement_requests
                .lock()
                .expect("replacement requests")
                .is_empty()
        );
        drop(requests);
        cleanup(supervisor, store_root, workspace);
    }

    #[tokio::test]
    async fn concurrent_follow_ups_reserve_an_idle_instance_only_once() {
        let initial: Arc<dyn Model> = Arc::new(ConstantModel::new("ready"));
        let roles = roles_with("race", |_| initial.clone());
        let (supervisor, store_root, workspace) = test_supervisor("send-race", roles);
        let instance = supervisor
            .spawn_instance(SubagentRole::Explore, "initial".to_string())
            .await
            .expect("spawn instance");
        supervisor
            .wait_instances(vec![instance.id.clone()], Duration::from_secs(2))
            .await
            .expect("initial run finishes");
        let document = supervisor
            .document(&instance.id)
            .await
            .expect("load instance document");
        let pending = PendingStreamModel::default();
        supervisor
            .register_model_runtime(Arc::new(pending.clone()), document.model, limits())
            .await;

        let (left, right) = tokio::join!(
            supervisor.send_message(instance.id.clone(), "left".to_string()),
            supervisor.send_message(instance.id.clone(), "right".to_string()),
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        wait_for_count(&pending.started, 1).await;
        supervisor
            .cancel_instance(instance.id)
            .await
            .expect("cancel reserved follow-up");
        wait_until_idle(&supervisor).await;
        cleanup(supervisor, store_root, workspace);
    }

    #[tokio::test]
    async fn persistent_summary_is_truncated_on_unicode_boundaries() {
        let model: Arc<dyn Model> = Arc::new(ConstantModel::new(
            "界".repeat(MAX_SUBAGENT_RESULT_CHARS + 1),
        ));
        let roles = roles_with("summary", |_| model.clone());
        let (supervisor, store_root, workspace) = test_supervisor("summary", roles);
        let instance = supervisor
            .spawn_instance(SubagentRole::Explore, "summarize".to_string())
            .await
            .expect("spawn instance");
        let snapshot = supervisor
            .wait_instances(vec![instance.id], Duration::from_secs(2))
            .await
            .expect("run finishes")
            .pop()
            .expect("snapshot");
        let summary = snapshot.latest_summary.expect("summary");
        assert!(summary.truncated);
        assert_eq!(
            summary.result.expect("summary result").chars().count(),
            MAX_SUBAGENT_RESULT_CHARS
        );
        cleanup(supervisor, store_root, workspace);
    }
}
