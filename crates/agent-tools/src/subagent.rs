use super::*;

pub const DELEGATE_TASK_TOOL_NAME: &str = "delegate_task";
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";
pub const SEND_SUBAGENT_TOOL_NAME: &str = "send_subagent";
pub const INSPECT_SUBAGENT_TOOL_NAME: &str = "inspect_subagent";
pub const WAIT_SUBAGENTS_TOOL_NAME: &str = "wait_subagents";
pub const CANCEL_SUBAGENT_TOOL_NAME: &str = "cancel_subagent";
pub const MAX_SUBAGENT_TASK_CHARS: usize = 4_000;
pub const MAX_SUBAGENT_WAIT_SECS: u64 = 300;
static SUBAGENT_NAME_SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn effective_subagent_permissions(
    parent: PermissionProfile,
    role: SubagentRole,
) -> PermissionProfile {
    let ceiling = match role {
        SubagentRole::Explore | SubagentRole::Plan => PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        },
        SubagentRole::Worker => PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Prompt,
        },
        SubagentRole::Reviewer => PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Prompt,
        },
    };
    PermissionProfile {
        mode: minimum_permission_mode(parent.mode, ceiling.mode),
        shell: minimum_shell_policy(parent.shell, ceiling.shell),
    }
}

fn minimum_permission_mode(left: PermissionMode, right: PermissionMode) -> PermissionMode {
    use PermissionMode::{DangerFullAccess, ReadOnly, WorkspaceWrite};
    match (left, right) {
        (ReadOnly, _) | (_, ReadOnly) => ReadOnly,
        (WorkspaceWrite, _) | (_, WorkspaceWrite) => WorkspaceWrite,
        (DangerFullAccess, DangerFullAccess) => DangerFullAccess,
    }
}

fn minimum_shell_policy(left: ShellPolicy, right: ShellPolicy) -> ShellPolicy {
    use ShellPolicy::{Allow, Deny, Prompt};
    match (left, right) {
        (Deny, _) | (_, Deny) => Deny,
        (Prompt, _) | (_, Prompt) => Prompt,
        (Allow, Allow) => Allow,
    }
}

pub trait SubagentExecutor: Send + Sync {
    fn execute(
        &self,
        task: String,
        cancellation: CancellationToken,
    ) -> BoxFuture<'static, SubagentExecutionSummary>;
}

pub trait SubagentController: Send + Sync {
    fn writer_lease(&self) -> Option<Arc<Semaphore>> {
        None
    }

    fn spawn(
        &self,
        role: SubagentRole,
        task: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>>;

    fn send(
        &self,
        instance_id: String,
        message: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>>;

    fn inspect(
        &self,
        instance_id: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<SubagentInstanceSnapshot>, String>>;

    fn wait(
        &self,
        instance_ids: Vec<String>,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<Vec<SubagentInstanceSnapshot>, String>>;

    fn cancel(
        &self,
        instance_id: String,
    ) -> BoxFuture<'static, Result<SubagentInstanceSnapshot, String>>;
}

#[derive(Clone)]
pub(crate) struct SubagentLifecycleTools {
    pub(crate) controller: Arc<dyn SubagentController>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnSubagentArgs {
    role: SubagentRole,
    task: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendSubagentArgs {
    instance_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectSubagentArgs {
    #[serde(default)]
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitSubagentsArgs {
    instance_ids: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelSubagentArgs {
    instance_id: String,
}

#[derive(Clone)]
pub(crate) struct DelegateTaskTool {
    pub(crate) executor: Arc<dyn SubagentExecutor>,
    pub(crate) identities: Arc<Mutex<SubagentIdentityAllocator>>,
}

pub(crate) struct SubagentIdentityAllocator {
    seed: u64,
    assigned: HashMap<String, SubagentIdentity>,
    pool: Vec<SubagentIdentity>,
    available: Vec<SubagentIdentity>,
}

impl SubagentIdentityAllocator {
    pub(crate) fn new(identities: &[SubagentIdentity]) -> Self {
        Self::with_seed(subagent_name_seed(), identities)
    }

    pub(crate) fn with_seed(seed: u64, identities: &[SubagentIdentity]) -> Self {
        let available = if identities.is_empty() {
            default_subagent_identities()
        } else {
            identities.to_vec()
        };
        Self {
            seed,
            assigned: HashMap::new(),
            pool: available.clone(),
            available,
        }
    }

    pub(crate) fn identity_for(&mut self, call_id: &str) -> SubagentIdentity {
        if let Some(identity) = self.assigned.get(call_id) {
            return identity.clone();
        }

        let mut hasher = DefaultHasher::new();
        self.seed.hash(&mut hasher);
        call_id.hash(&mut hasher);
        self.assigned.len().hash(&mut hasher);
        let hash = hasher.finish() as usize;
        let identity = if self.available.is_empty() {
            self.pool[hash % self.pool.len()].clone()
        } else {
            let index = hash % self.available.len();
            self.available.swap_remove(index)
        };
        self.assigned.insert(call_id.to_string(), identity.clone());
        identity
    }
}

impl DelegateTaskTool {
    fn agent_identity(&self, call_id: &str) -> SubagentIdentity {
        self.identities
            .lock()
            .expect("subagent identity allocator lock poisoned")
            .identity_for(call_id)
    }
}

fn subagent_name_seed() -> u64 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    timestamp ^ SUBAGENT_NAME_SEED_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateTaskArgs {
    task: String,
}

#[derive(Serialize)]
struct DelegateTaskOutput<'a> {
    ok: bool,
    agent_id: &'a str,
    agent_name: &'a str,
    task: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    model_calls: usize,
    tool_calls: usize,
    truncated: bool,
}

#[async_trait]
impl Tool for DelegateTaskTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![delegate_task_definition()]
    }

    fn execution_kind(&self, call: &ToolCall) -> ToolExecutionKind {
        ToolExecutionKind::Subagent {
            task: delegate_task_label(call),
            identity: self.agent_identity(&call.id),
        }
    }

    async fn execute(
        &self,
        call: ToolCall,
        _approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        let identity = self.agent_identity(&call.id);
        let summary = match parse_delegate_task(&call) {
            Ok(task) if context.cancellation.is_cancelled() => {
                SubagentExecutionSummary::failure(task, "subagent execution cancelled", 0, 0)
            }
            Ok(task) => self.executor.execute(task, context.cancellation).await,
            Err(error) => {
                SubagentExecutionSummary::failure(delegate_task_label(&call), error, 0, 0)
            }
        }
        .with_agent_identity(&identity);
        let ok = summary.error.is_none();
        let error = summary.error.clone();
        let content = serde_json::to_string(&DelegateTaskOutput {
            ok,
            agent_id: &identity.id,
            agent_name: &identity.name,
            task: &summary.task,
            result: summary.result.as_deref(),
            error: summary.error.as_deref(),
            model_calls: summary.model_calls,
            tool_calls: summary.tool_calls,
            truncated: summary.truncated,
        })
        .expect("subagent tool output must serialize");

        ToolExecution::Completed(ToolResult {
            ok,
            content,
            error,
            summary: Some(ToolExecutionSummary::subagent(summary)),
        })
    }
}

#[async_trait]
impl Tool for SubagentLifecycleTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        subagent_lifecycle_definitions()
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    async fn execute(
        &self,
        call: ToolCall,
        _approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        if context.cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }

        let result = match call.function.name.as_str() {
            SPAWN_SUBAGENT_TOOL_NAME => match parse_args::<SpawnSubagentArgs>(&call)
                .and_then(|args| validate_subagent_message(args.task).map(|task| (args.role, task)))
            {
                Ok((role, task)) => self
                    .controller
                    .spawn(role, task)
                    .await
                    .map(|snapshot| json!({"instance": snapshot})),
                Err(error) => Err(error),
            },
            SEND_SUBAGENT_TOOL_NAME => {
                match parse_args::<SendSubagentArgs>(&call).and_then(|args| {
                    let instance_id = validate_instance_id(args.instance_id)?;
                    let message = validate_subagent_message(args.message)?;
                    Ok((instance_id, message))
                }) {
                    Ok((instance_id, message)) => self
                        .controller
                        .send(instance_id, message)
                        .await
                        .map(|snapshot| json!({"instance": snapshot})),
                    Err(error) => Err(error),
                }
            }
            INSPECT_SUBAGENT_TOOL_NAME => match parse_args::<InspectSubagentArgs>(&call)
                .and_then(|args| args.instance_id.map(validate_instance_id).transpose())
            {
                Ok(instance_id) => self
                    .controller
                    .inspect(instance_id)
                    .await
                    .map(|instances| json!({"instances": instances})),
                Err(error) => Err(error),
            },
            WAIT_SUBAGENTS_TOOL_NAME => {
                match parse_args::<WaitSubagentsArgs>(&call).and_then(|args| {
                    if args.instance_ids.is_empty() || args.instance_ids.len() > 8 {
                        return Err("instance_ids must contain between 1 and 8 values".to_string());
                    }
                    let mut seen = HashSet::new();
                    let mut ids = Vec::with_capacity(args.instance_ids.len());
                    for id in args.instance_ids {
                        let id = validate_instance_id(id)?;
                        if !seen.insert(id.clone()) {
                            return Err(format!("duplicate instance id {id:?}"));
                        }
                        ids.push(id);
                    }
                    let timeout_secs = args.timeout_secs.unwrap_or(MAX_SUBAGENT_WAIT_SECS);
                    if timeout_secs > MAX_SUBAGENT_WAIT_SECS {
                        return Err(format!(
                            "timeout_secs must not exceed {MAX_SUBAGENT_WAIT_SECS}"
                        ));
                    }
                    Ok((ids, Duration::from_secs(timeout_secs)))
                }) {
                    Ok((ids, timeout)) => self
                        .controller
                        .wait(ids, timeout)
                        .await
                        .map(|instances| json!({"instances": instances})),
                    Err(error) => Err(error),
                }
            }
            CANCEL_SUBAGENT_TOOL_NAME => match parse_args::<CancelSubagentArgs>(&call)
                .and_then(|args| validate_instance_id(args.instance_id))
            {
                Ok(instance_id) => self
                    .controller
                    .cancel(instance_id)
                    .await
                    .map(|snapshot| json!({"instance": snapshot})),
                Err(error) => Err(error),
            },
            name => Err(format!("unknown subagent lifecycle tool {name:?}")),
        };

        match result {
            Ok(value) => ToolExecution::Completed(ToolResult {
                ok: true,
                content: serde_json::to_string(&value)
                    .expect("subagent lifecycle output must serialize"),
                error: None,
                summary: None,
            }),
            Err(error) => ToolExecution::Completed(ToolResult::error(error)),
        }
    }
}

fn validate_subagent_message(value: String) -> Result<String, String> {
    let value = value.trim().to_string();
    let chars = value.chars().count();
    if chars == 0 || chars > MAX_SUBAGENT_TASK_CHARS {
        return Err(format!(
            "subagent message must contain between 1 and {MAX_SUBAGENT_TASK_CHARS} characters"
        ));
    }
    Ok(value)
}

fn validate_instance_id(value: String) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("invalid subagent instance id {value:?}"));
    }
    Ok(value)
}

pub(crate) fn delegate_task_definition() -> ToolDefinition {
    ToolDefinition::function(
        DELEGATE_TASK_TOOL_NAME,
        "Delegate one self-contained, read-only workspace investigation to an isolated subagent. The call waits for the result. Issue multiple delegate_task calls in the same response when independent investigations can run in parallel; use direct tools for simple lookups.",
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SUBAGENT_TASK_CHARS,
                    "description": format!("Self-contained investigation task for the subagent: what to look at and what to report back (at most {MAX_SUBAGENT_TASK_CHARS} characters).")
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    )
}

pub(crate) fn subagent_lifecycle_definitions() -> Vec<ToolDefinition> {
    let role_schema = json!({
        "type": "string",
        "enum": ["explore", "plan", "worker", "reviewer"],
        "description": "Subagent role, which determines its tool profile: explore and plan are read-only investigators, worker can edit files and run commands, reviewer reads and runs commands."
    });
    vec![
        ToolDefinition::function(
            SPAWN_SUBAGENT_TOOL_NAME,
            "Start a persistent session-scoped subagent in the background. Returns immediately with its instance and run identifiers.",
            json!({
                "type": "object",
                "properties": {
                    "role": role_schema,
                    "task": {"type": "string", "minLength": 1, "maxLength": MAX_SUBAGENT_TASK_CHARS, "description": format!("Initial task for the subagent (at most {MAX_SUBAGENT_TASK_CHARS} characters).")}
                },
                "required": ["role", "task"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            SEND_SUBAGENT_TOOL_NAME,
            "Send a follow-up message to an existing persistent subagent and start its next background run.",
            json!({
                "type": "object",
                "properties": {
                    "instance_id": {"type": "string", "minLength": 1, "description": "Instance identifier returned by spawn_subagent."},
                    "message": {"type": "string", "minLength": 1, "maxLength": MAX_SUBAGENT_TASK_CHARS, "description": format!("Follow-up instruction for the subagent's next run (at most {MAX_SUBAGENT_TASK_CHARS} characters).")}
                },
                "required": ["instance_id", "message"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            INSPECT_SUBAGENT_TOOL_NAME,
            "Inspect one persistent subagent or list all persistent subagents in the current session. Only bounded summaries are returned.",
            json!({
                "type": "object",
                "properties": {
                    "instance_id": {"type": "string", "minLength": 1, "description": "Inspect only this subagent instance. Omit to list all subagents in the session."}
                },
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            WAIT_SUBAGENTS_TOOL_NAME,
            "Wait for one or more persistent subagents to stop running. A timeout returns current statuses without cancelling them.",
            json!({
                "type": "object",
                "properties": {
                    "instance_ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 8,
                        "uniqueItems": true,
                        "items": {"type": "string", "minLength": 1},
                        "description": "Instance identifiers of the subagents to wait for (1 to 8 entries)."
                    },
                    "timeout_secs": {"type": "integer", "minimum": 0, "maximum": MAX_SUBAGENT_WAIT_SECS, "description": format!("Maximum time to wait in seconds (0..={MAX_SUBAGENT_WAIT_SECS}). Defaults to {MAX_SUBAGENT_WAIT_SECS}; on timeout the call returns the current statuses without cancelling anything.")}
                },
                "required": ["instance_ids"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            CANCEL_SUBAGENT_TOOL_NAME,
            "Cancel a queued or running persistent subagent. Cancelling an already idle or terminal instance is a successful no-op.",
            json!({
                "type": "object",
                "properties": {
                    "instance_id": {"type": "string", "minLength": 1, "description": "Instance identifier of the subagent to cancel."}
                },
                "required": ["instance_id"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn parse_delegate_task(call: &ToolCall) -> Result<String, String> {
    let args = parse_args::<DelegateTaskArgs>(call)?;
    let task = args.task.trim().to_string();
    if task.is_empty() {
        return Err("task must not be empty".to_string());
    }
    let length = task.chars().count();
    if length > MAX_SUBAGENT_TASK_CHARS {
        return Err(format!(
            "task must not exceed {MAX_SUBAGENT_TASK_CHARS} characters (received {length})"
        ));
    }
    Ok(task)
}

fn delegate_task_label(call: &ToolCall) -> String {
    serde_json::from_str::<DelegateTaskArgs>(&call.function.arguments)
        .ok()
        .map(|args| {
            args.task
                .trim()
                .chars()
                .take(MAX_SUBAGENT_TASK_CHARS)
                .collect()
        })
        .filter(|task: &String| !task.is_empty())
        .unwrap_or_else(|| "invalid delegated task".to_string())
}
