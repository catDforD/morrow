use super::*;

pub(crate) const TOOL_CANCELLED_ERROR: &str = "tool execution cancelled";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInToolAllowlist {
    names: HashSet<String>,
}

impl BuiltInToolAllowlist {
    pub fn all() -> Self {
        Self::new([
            READ_FILE_TOOL_NAME,
            LIST_FILES_TOOL_NAME,
            SEARCH_TEXT_TOOL_NAME,
            EDIT_FILE_TOOL_NAME,
            WRITE_FILE_TOOL_NAME,
            APPLY_PATCH_TOOL_NAME,
            SHELL_COMMAND_TOOL_NAME,
            WEB_FETCH_TOOL_NAME,
        ])
    }

    pub fn research() -> Self {
        Self::new([
            READ_FILE_TOOL_NAME,
            LIST_FILES_TOOL_NAME,
            SEARCH_TEXT_TOOL_NAME,
            WEB_FETCH_TOOL_NAME,
        ])
    }

    pub fn for_subagent(role: SubagentRole, permissions: PermissionProfile) -> Self {
        let mut names = match role {
            SubagentRole::Explore | SubagentRole::Plan => Self::research().names,
            SubagentRole::Worker => Self::all().names,
            SubagentRole::Reviewer => {
                Self::new([
                    READ_FILE_TOOL_NAME,
                    LIST_FILES_TOOL_NAME,
                    SEARCH_TEXT_TOOL_NAME,
                    SHELL_COMMAND_TOOL_NAME,
                    WEB_FETCH_TOOL_NAME,
                ])
                .names
            }
        };
        if permissions.mode == PermissionMode::ReadOnly {
            names.remove(EDIT_FILE_TOOL_NAME);
            names.remove(WRITE_FILE_TOOL_NAME);
            names.remove(APPLY_PATCH_TOOL_NAME);
        }
        if permissions.shell == ShellPolicy::Deny {
            names.remove(SHELL_COMMAND_TOOL_NAME);
        }
        Self { names }
    }

    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// 按 `[tools] allow/deny` 裁剪内置工具集合。
    pub fn filtered(&self, tools: &ToolsConfig) -> Self {
        Self {
            names: self
                .names
                .iter()
                .filter(|name| tools.allows(name))
                .cloned()
                .collect(),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error(transparent)]
    PermissionEvaluator(#[from] PermissionEvaluatorError),
    #[error("duplicate tool registered: {name}")]
    DuplicateTool { name: String },
    #[error("mcp server {server}: {message}")]
    McpServer { server: String, message: String },
    #[error("failed to initialize web_fetch: {0}")]
    WebFetch(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execution_kind(&self, _call: &ToolCall) -> ToolExecutionKind {
        ToolExecutionKind::Standard
    }

    async fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution;
}

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    tool: Arc<dyn Tool>,
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

#[derive(Debug, Clone)]
pub struct ToolRegistryBuild {
    pub registry: ToolRegistry,
    pub diagnostics: Vec<String>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tools = self
            .tools
            .iter()
            .map(|registered| registered.definition.function.name.as_str())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("ToolRegistry")
            .field("tools", &tools)
            .finish()
    }
}

impl ToolRegistry {
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn built_in(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
    ) -> Result<Self, ToolRegistryError> {
        Self::built_in_with_allowlist(root, permissions, BuiltInToolAllowlist::all())
    }

    pub fn built_in_with_allowlist(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        allowed: BuiltInToolAllowlist,
    ) -> Result<Self, ToolRegistryError> {
        Self::built_in_with_allowlist_and_writer_lease(root, permissions, allowed, None)
    }

    pub fn built_in_with_allowlist_and_writer_lease(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        allowed: BuiltInToolAllowlist,
        writer_lease: Option<Arc<Semaphore>>,
    ) -> Result<Self, ToolRegistryError> {
        Self::built_in_with_allowlist_and_writer_lease_and_artifact_root(
            root,
            permissions,
            allowed,
            writer_lease,
            None,
            true,
        )
    }

    pub fn built_in_with_allowlist_and_writer_lease_and_artifact_root(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        allowed: BuiltInToolAllowlist,
        writer_lease: Option<Arc<Semaphore>>,
        artifact_root: Option<PathBuf>,
        auto_approve_workspace_writes: bool,
    ) -> Result<Self, ToolRegistryError> {
        let evaluator = PermissionEvaluator::new_with_read_roots(
            root,
            permissions,
            artifact_root.iter().cloned(),
        )?
        .with_auto_approve_workspace_writes(auto_approve_workspace_writes);
        let mut registry = Self::empty();
        registry.register(Arc::new(BuiltInTools {
            evaluator,
            allowed: allowed.clone(),
            writer_lease,
        }))?;
        if allowed.contains(WEB_FETCH_TOOL_NAME) {
            registry.register(Arc::new(
                WebFetchTool::new(artifact_root).map_err(ToolRegistryError::WebFetch)?,
            ))?;
        }
        Ok(registry)
    }

    pub fn research(root: impl Into<PathBuf>) -> Result<Self, ToolRegistryError> {
        Self::research_with_artifact_root(root, None)
    }

    pub fn research_with_artifact_root(
        root: impl Into<PathBuf>,
        artifact_root: Option<PathBuf>,
    ) -> Result<Self, ToolRegistryError> {
        Self::built_in_with_allowlist_and_writer_lease_and_artifact_root(
            root,
            PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Deny,
            },
            BuiltInToolAllowlist::research(),
            None,
            artifact_root,
            true,
        )
    }

    pub async fn with_mcp_cache_async(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        mcp_servers: &[McpServerConfig],
        mcp_cache: &McpToolCache,
    ) -> Result<ToolRegistryBuild, ToolRegistryError> {
        Self::with_mcp_cache_and_writer_lease_async(root, permissions, mcp_servers, mcp_cache, None)
            .await
    }

    pub async fn with_mcp_cache_and_writer_lease_async(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        mcp_servers: &[McpServerConfig],
        mcp_cache: &McpToolCache,
        writer_lease: Option<Arc<Semaphore>>,
    ) -> Result<ToolRegistryBuild, ToolRegistryError> {
        Self::with_mcp_cache_and_writer_lease_and_artifact_root_async(
            root,
            permissions,
            mcp_servers,
            mcp_cache,
            writer_lease,
            None,
        )
        .await
    }

    pub async fn with_mcp_cache_and_writer_lease_and_artifact_root_async(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        mcp_servers: &[McpServerConfig],
        mcp_cache: &McpToolCache,
        writer_lease: Option<Arc<Semaphore>>,
        artifact_root: Option<PathBuf>,
    ) -> Result<ToolRegistryBuild, ToolRegistryError> {
        Self::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            root,
            permissions,
            mcp_servers,
            mcp_cache,
            writer_lease,
            artifact_root,
            &ToolsConfig::default(),
            true,
        )
        .await
    }

    // 逐级透传的装配参数本就偏多，引入 options 结构体重构留给后续统一处理。
    #[allow(clippy::too_many_arguments)]
    pub async fn with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
        mcp_servers: &[McpServerConfig],
        mcp_cache: &McpToolCache,
        writer_lease: Option<Arc<Semaphore>>,
        artifact_root: Option<PathBuf>,
        tools: &ToolsConfig,
        auto_approve_workspace_writes: bool,
    ) -> Result<ToolRegistryBuild, ToolRegistryError> {
        let root = root.into();
        let mut registry = Self::built_in_with_allowlist_and_writer_lease_and_artifact_root(
            &root,
            permissions,
            BuiltInToolAllowlist::all().filtered(tools),
            writer_lease,
            artifact_root,
            auto_approve_workspace_writes,
        )?;
        let discovery = mcp::discover_tools_with_filter(&root, mcp_servers, mcp_cache, tools).await;
        for tool in discovery.tools {
            registry.register(tool)?;
        }
        Ok(ToolRegistryBuild {
            registry,
            diagnostics: discovery.diagnostics,
        })
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let definitions = tool.definitions();
        let mut new_names = HashSet::new();
        for definition in &definitions {
            let name = definition.function.name.clone();
            if !new_names.insert(name.clone())
                || self.tools.iter().any(|registered| {
                    registered.definition.function.name == definition.function.name
                })
            {
                return Err(ToolRegistryError::DuplicateTool { name });
            }
        }

        for definition in definitions {
            self.tools.push(RegisteredTool {
                definition,
                tool: tool.clone(),
            });
        }
        Ok(())
    }

    pub fn register_subagent(
        &mut self,
        executor: Arc<dyn SubagentExecutor>,
        identities: &[SubagentIdentity],
    ) -> Result<(), ToolRegistryError> {
        self.register(Arc::new(DelegateTaskTool {
            executor,
            identities: Arc::new(Mutex::new(SubagentIdentityAllocator::new(identities))),
        }))
    }

    pub fn register_subagent_controller(
        &mut self,
        controller: Arc<dyn SubagentController>,
    ) -> Result<(), ToolRegistryError> {
        self.register(Arc::new(SubagentLifecycleTools { controller }))
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|registered| registered.definition.clone())
            .collect()
    }

    pub fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        self.tools
            .iter()
            .find(|registered| registered.definition.function.name == call.function.name)
            .map(|registered| registered.tool.execution_mode(call))
            .unwrap_or(ToolExecutionMode::Concurrent)
    }

    pub fn execution_kind(&self, call: &ToolCall) -> ToolExecutionKind {
        self.tools
            .iter()
            .find(|registered| registered.definition.function.name == call.function.name)
            .map(|registered| registered.tool.execution_kind(call))
            .unwrap_or(ToolExecutionKind::Standard)
    }

    pub async fn execute<C>(&self, call: C) -> ToolExecution
    where
        C: Borrow<ToolCall> + Send,
    {
        self.execute_with_context(call, ToolExecutionContext::default())
            .await
    }

    pub async fn execute_with_context<C>(
        &self,
        call: C,
        context: ToolExecutionContext,
    ) -> ToolExecution
    where
        C: Borrow<ToolCall> + Send,
    {
        let call = call.borrow().clone();
        self.execute_inner(call, None, context).await
    }

    pub async fn execute_approved<C, D, R>(&self, call: C, decision: D, request: R) -> ToolExecution
    where
        C: Borrow<ToolCall> + Send,
        D: Borrow<ApprovalDecision> + Send,
        R: Borrow<ApprovalRequest> + Send,
    {
        self.execute_approved_with_context(call, decision, request, ToolExecutionContext::default())
            .await
    }

    pub async fn execute_approved_with_context<C, D, R>(
        &self,
        call: C,
        decision: D,
        request: R,
        context: ToolExecutionContext,
    ) -> ToolExecution
    where
        C: Borrow<ToolCall> + Send,
        D: Borrow<ApprovalDecision> + Send,
        R: Borrow<ApprovalRequest> + Send,
    {
        let call = call.borrow().clone();
        let decision = decision.borrow().clone();
        let request = request.borrow().clone();
        self.execute_inner(call, Some(ToolApproval { decision, request }), context)
            .await
    }

    async fn execute_inner(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        match self
            .tools
            .iter()
            .find(|registered| registered.definition.function.name == call.function.name)
        {
            Some(registered) => registered.tool.execute(call, approval, context).await,
            None => ToolExecution::error(self.unknown_tool_message(&call.function.name)),
        }
    }

    fn unknown_tool_message(&self, name: &str) -> String {
        let mut available = self
            .tools
            .iter()
            .map(|registered| registered.definition.function.name.as_str())
            .collect::<Vec<_>>();
        available.sort_unstable();
        let mut message = format!(
            "unknown tool {name:?}. Available tools: {}",
            available.join(", ")
        );
        let suggestions = available
            .iter()
            .copied()
            .filter(|candidate| tool_names_overlap(name, candidate))
            .take(3)
            .collect::<Vec<_>>();
        if !suggestions.is_empty() {
            message.push_str(&format!(". Did you mean: {}?", suggestions.join(", ")));
        }
        message
    }
}

fn tool_names_overlap(requested: &str, candidate: &str) -> bool {
    if requested.is_empty() {
        return false;
    }
    candidate.contains(requested)
        || requested.contains(candidate)
        || common_prefix_len(requested, candidate) >= 3
}

fn common_prefix_len(left: &str, right: &str) -> usize {
    left.bytes()
        .zip(right.bytes())
        .take_while(|(left, right)| left == right)
        .count()
}

impl ToolRuntime for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ToolRegistry::definitions(self)
    }

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        ToolRegistry::execution_mode(self, call)
    }

    fn execution_kind(&self, call: &ToolCall) -> ToolExecutionKind {
        ToolRegistry::execution_kind(self, call)
    }

    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture {
        let registry = self.clone();
        async move { registry.execute_inner(call, approval, context).await }.boxed()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BuiltInTools {
    pub(crate) evaluator: PermissionEvaluator,
    pub(crate) allowed: BuiltInToolAllowlist,
    pub(crate) writer_lease: Option<Arc<Semaphore>>,
}

#[async_trait]
impl Tool for BuiltInTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        built_in_definitions()
            .into_iter()
            .filter(|definition| self.allowed.contains(&definition.function.name))
            .collect()
    }

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        match call.function.name.as_str() {
            "read_file" | "list_files" | "search_text" => ToolExecutionMode::Concurrent,
            "edit_file" | "write_file" | "apply_patch" | "shell_command" => {
                ToolExecutionMode::Serial
            }
            _ => ToolExecutionMode::Concurrent,
        }
    }

    async fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        if context.cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }

        if !self.allowed.contains(&call.function.name) {
            return ToolExecution::error(format!(
                "tool {:?} is not available in the active tool profile",
                call.function.name
            ));
        }

        let _writer_permit = if matches!(
            call.function.name.as_str(),
            EDIT_FILE_TOOL_NAME
                | WRITE_FILE_TOOL_NAME
                | APPLY_PATCH_TOOL_NAME
                | SHELL_COMMAND_TOOL_NAME
        ) {
            let Some(lease) = self.writer_lease.clone() else {
                return self
                    .execute_without_writer_lease(call, approval, context)
                    .await;
            };
            let permit = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return ToolExecution::error(TOOL_CANCELLED_ERROR);
                }
                permit = lease.acquire_owned() => permit,
            };
            match permit {
                Ok(permit) => Some(permit),
                Err(_) => return ToolExecution::error("workspace writer lease is unavailable"),
            }
        } else {
            None
        };

        self.execute_without_writer_lease(call, approval, context)
            .await
    }
}

impl BuiltInTools {
    async fn execute_without_writer_lease(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        if call.function.name == "shell_command" {
            let approval = approval.as_ref().map(|approval| {
                (
                    &approval.decision as &ApprovalDecision,
                    &approval.request as &ApprovalRequest,
                )
            });
            return self
                .shell_command(&call, approval, &context.cancellation)
                .await;
        }

        let tools = self.clone();
        let cancellation = context.cancellation;
        tokio::task::spawn_blocking(move || {
            let approval = approval.as_ref().map(|approval| {
                (
                    &approval.decision as &ApprovalDecision,
                    &approval.request as &ApprovalRequest,
                )
            });
            tools.execute_blocking(&call, approval, &cancellation)
        })
        .await
        .unwrap_or_else(|error| {
            ToolExecution::error(format!("tool execution task failed: {error}"))
        })
    }
}

impl BuiltInTools {
    fn execute_blocking(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        if cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }

        let result = match call.function.name.as_str() {
            "read_file" => self.read_file(call).map(tool_ok),
            "list_files" => self.list_files(call).map(tool_ok),
            "search_text" => self.search_text(call).map(tool_ok),
            "edit_file" => return self.edit_file(call, approval, cancellation),
            "write_file" => return self.write_file(call, approval, cancellation),
            "apply_patch" => return self.apply_patch(call, approval, cancellation),
            "shell_command" => {
                return ToolExecution::error("shell command must use the async execution path");
            }
            name => Err(format!("unknown tool {name:?}")),
        };

        match result {
            Ok(result) => ToolExecution::Completed(result),
            Err(error) => ToolExecution::Completed(tool_error(error)),
        }
    }

    fn read_file(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<ReadFileArgs>(call)?;
        let start_line = args.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err("start_line must be at least 1".to_string());
        }
        let max_lines = clamp_limit(args.max_lines, DEFAULT_READ_LINES, MAX_READ_LINES)?;
        let path = self.resolve_existing_path(&args.path)?;
        if !path.is_file() {
            return Err(format!("{} is not a file", self.display_path(&path)));
        }

        let content = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", self.display_path(&path)))?;
        let lines = content.lines().collect::<Vec<_>>();
        let selected = lines
            .iter()
            .skip(start_line.saturating_sub(1))
            .take(max_lines)
            .copied()
            .collect::<Vec<_>>();
        let end_line = (!selected.is_empty()).then_some(start_line + selected.len() - 1);
        let truncated = start_line.saturating_sub(1) + selected.len() < lines.len();

        Ok(json!({
            "path": self.display_path(&path),
            "start_line": start_line,
            "end_line": end_line,
            "total_lines": lines.len(),
            "truncated": truncated,
            "content": selected.join("\n"),
        }))
    }

    fn list_files(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<ListFilesArgs>(call)?;
        let path = args.path.unwrap_or_else(|| ".".to_string());
        let recursive = args.recursive.unwrap_or(false);
        let max_entries = clamp_limit(args.max_entries, DEFAULT_LIST_ENTRIES, MAX_LIST_ENTRIES)?;
        let path = self.resolve_existing_path(&path)?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory", self.display_path(&path)));
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        self.collect_entries(&path, recursive, max_entries, &mut entries, &mut truncated)?;

        Ok(json!({
            "path": self.display_path(&path),
            "recursive": recursive,
            "truncated": truncated,
            "entries": entries,
        }))
    }

    fn search_text(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<SearchTextArgs>(call)?;
        if args.query.is_empty() {
            return Err("query must not be empty".to_string());
        }
        let path = args.path.unwrap_or_else(|| ".".to_string());
        let path = self.resolve_existing_path(&path)?;
        let max_results =
            clamp_limit(args.max_results, DEFAULT_SEARCH_RESULTS, MAX_SEARCH_RESULTS)?;
        let case_sensitive = args.case_sensitive.unwrap_or(false);
        let options = SearchOptions {
            query: &args.query,
            case_sensitive,
            max_results,
        };

        if let Some(ripgrep) = ripgrep_binary() {
            match self.search_text_with_ripgrep(&ripgrep, &path, &options) {
                Ok(output) => return Ok(output.into_value()),
                Err(RipgrepSearchError::Unavailable) => {}
                Err(RipgrepSearchError::Failed(error)) => return Err(error),
            }
        }

        Ok(self.search_text_fallback(&path, &options)?.into_value())
    }

    pub(crate) fn search_text_fallback(
        &self,
        path: &Path,
        options: &SearchOptions<'_>,
    ) -> Result<SearchOutput, String> {
        let mut output = SearchOutput::new(
            options.query,
            self.display_path(path),
            options.case_sensitive,
            options.max_results,
        );

        if path.is_file() {
            self.search_file(path, options, true, &mut output)?;
        } else if path.is_dir() {
            let mut files = Vec::new();
            self.collect_search_files(path, &mut files)?;
            for file in files {
                self.search_file(&file, options, false, &mut output)?;
                if output.result_truncated {
                    break;
                }
            }
        } else {
            return Err(format!("{} is not searchable", self.display_path(path)));
        }

        Ok(output)
    }

    fn search_text_with_ripgrep(
        &self,
        ripgrep: &Path,
        path: &Path,
        options: &SearchOptions<'_>,
    ) -> Result<SearchOutput, RipgrepSearchError> {
        let evaluator = self.evaluator().map_err(RipgrepSearchError::Failed)?;
        let search_path = self.display_path(path);
        let mut output = SearchOutput::new(
            options.query,
            search_path.clone(),
            options.case_sensitive,
            options.max_results,
        );
        let mut command = StdCommand::new(ripgrep);
        command
            .current_dir(evaluator.root())
            .arg("--json")
            .arg("--fixed-strings")
            .arg("--color")
            .arg("never")
            .arg("--no-messages");
        if !options.case_sensitive {
            command.arg("--ignore-case");
        }
        for skipped in SEARCH_SKIP_NAMES {
            command.arg("--glob").arg(format!("!**/{skipped}/**"));
            command.arg("--glob").arg(format!("!{skipped}/**"));
        }
        command
            .arg(options.query)
            .arg(search_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RipgrepSearchError::Unavailable
            } else {
                RipgrepSearchError::Failed(format!("failed to start ripgrep: {err}"))
            }
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RipgrepSearchError::Failed("failed to capture ripgrep stdout".to_string())
        })?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut stopped_early = false;

        loop {
            line.clear();
            let read = reader.read_line(&mut line).map_err(|err| {
                RipgrepSearchError::Failed(format!("failed to read ripgrep output: {err}"))
            })?;
            if read == 0 {
                break;
            }
            let frame = line.trim_end_matches(['\r', '\n']);
            if let Some(match_event) = parse_ripgrep_match(frame)? {
                let match_path = Path::new(&match_event.path);
                let display_path = if match_path.is_absolute() {
                    self.display_path(match_path)
                } else {
                    self.display_path(&evaluator.root().join(match_path))
                };
                if !output.push_match(display_path, match_event.line, match_event.text) {
                    stopped_early = true;
                    let _ = child.kill();
                    break;
                }
            }
        }

        let status = child.wait().map_err(|err| {
            RipgrepSearchError::Failed(format!("failed to wait for ripgrep: {err}"))
        })?;
        if !stopped_early && !matches!(status.code(), Some(0 | 1)) {
            return Err(RipgrepSearchError::Failed(format!(
                "ripgrep search failed with status {status}"
            )));
        }

        Ok(output)
    }

    fn edit_file(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_edit_file(call), approval, cancellation)
    }

    fn plan_edit_file(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<EditFileArgs>(call)?;
        if args.old_text.is_empty() {
            return Err("old_text must not be empty".to_string());
        }

        let path = self.resolve_write_path(&args.path)?;
        let metadata = fs::metadata(&path)
            .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", self.display_path(&path)));
        }

        let content = fs::read_to_string(&path).map_err(|err| {
            format!(
                "failed to read {} as UTF-8 text: {err}",
                self.display_path(&path)
            )
        })?;
        let replacements = content.matches(&args.old_text).count();
        if replacements != 1 {
            return Err(format!(
                "old_text must match exactly once in {}; found {replacements}",
                self.display_path(&path)
            ));
        }

        let updated = content.replacen(&args.old_text, &args.new_text, 1);
        let display_path = self.display_path(&path);
        let summary = FileChangeSummary {
            path: display_path.clone(),
            operation: FileChangeOperation::Update,
            replacements: 1,
            created: false,
            overwritten: true,
            deleted: false,
        };
        let change = StagedPatchChange::write(
            path,
            PatchOperationKind::Update,
            updated.clone(),
            Some(metadata.permissions()),
            summary,
            Some(content),
            Some(updated),
        );
        let data = json!({
            "path": display_path,
            "replacements": 1,
            "created": false,
            "overwritten": true,
        });

        self.file_change_plan(vec![change], data)
    }

    fn write_file(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_write_file(call), approval, cancellation)
    }

    fn plan_write_file(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<WriteFileArgs>(call)?;
        let overwrite = args.overwrite.unwrap_or(false);
        let path = self.resolve_write_path(&args.path)?;
        let existing = match fs::metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(format!(
                    "failed to inspect {}: {err}",
                    self.display_path(&path)
                ));
            }
        };

        if let Some(metadata) = existing.as_ref() {
            if !metadata.is_file() {
                return Err(format!("{} is not a file", self.display_path(&path)));
            }
            if !overwrite {
                return Err(format!(
                    "{} already exists; set overwrite to true to replace it",
                    self.display_path(&path)
                ));
            }
        }

        let created = existing.is_none();
        let overwritten = existing.is_some();
        let original = if overwritten {
            Some(fs::read_to_string(&path).map_err(|err| {
                format!(
                    "failed to read {} as UTF-8 text: {err}",
                    self.display_path(&path)
                )
            })?)
        } else {
            None
        };
        let permissions = existing.map(|metadata| metadata.permissions());
        let display_path = self.display_path(&path);
        let summary = FileChangeSummary {
            path: display_path.clone(),
            operation: if created {
                FileChangeOperation::Add
            } else {
                FileChangeOperation::Update
            },
            replacements: 0,
            created,
            overwritten,
            deleted: false,
        };
        let change = StagedPatchChange::write(
            path,
            if created {
                PatchOperationKind::Add
            } else {
                PatchOperationKind::Update
            },
            args.content.clone(),
            permissions,
            summary,
            original,
            Some(args.content),
        );

        let data = json!({
            "path": display_path,
            "replacements": 0,
            "created": created,
            "overwritten": overwritten,
        });

        self.file_change_plan(vec![change], data)
    }

    fn apply_patch(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_apply_patch(call), approval, cancellation)
    }

    fn plan_apply_patch(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<ApplyPatchArgs>(call)?;
        let operations = parse_patch(&args.patch)?;
        let changes = self.plan_patch_changes(operations)?;
        let files = changes
            .iter()
            .map(|change| file_change_summary_json(&change.summary))
            .collect::<Vec<_>>();
        let data = json!({
            "changed_files": files.len(),
            "files": files,
        });

        self.file_change_plan(changes, data)
    }

    fn execute_file_change_plan(
        &self,
        call: &ToolCall,
        plan: Result<FileChangePlan, String>,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => return ToolExecution::error(error),
        };
        if cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }
        let evaluator = match self.evaluator() {
            Ok(evaluator) => evaluator,
            Err(error) => return ToolExecution::error(error),
        };

        match evaluator.file_changes_decision(&call.id, plan.files.clone(), plan.diff.clone()) {
            PermissionDecision::Allow => self.commit_file_change_plan(plan, cancellation),
            PermissionDecision::Deny(error) => ToolExecution::error(error),
            PermissionDecision::Prompt(request) => match approval {
                None => ToolExecution::ApprovalRequired(request),
                Some((decision, original_request)) => {
                    if decision.request_id != original_request.id {
                        return ToolExecution::error(format!(
                            "approval decision {} does not match pending approval {}",
                            decision.request_id, original_request.id
                        ));
                    }
                    if original_request.id != request.id {
                        return ToolExecution::error(format!(
                            "approval request {} does not match required approval {}",
                            original_request.id, request.id
                        ));
                    }
                    if original_request.action != request.action {
                        return ToolExecution::error(
                            "file changes changed since approval request; approval no longer matches planned changes",
                        );
                    }
                    if !decision.approved {
                        return ToolExecution::error("file changes approval denied");
                    }
                    self.commit_file_change_plan(plan, cancellation)
                }
            },
        }
    }

    fn commit_file_change_plan(
        &self,
        plan: FileChangePlan,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        // 提交一旦开始就必须完整结束或回滚，因此只在进入事务前响应取消。
        if cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }
        match commit_patch_changes(plan.changes, self) {
            Ok(()) => ToolExecution::Completed(tool_ok_with_summary(plan.data, plan.summary)),
            Err(error) => ToolExecution::error(error),
        }
    }

    fn file_change_plan(
        &self,
        changes: Vec<StagedPatchChange>,
        data: Value,
    ) -> Result<FileChangePlan, String> {
        let files = changes
            .iter()
            .map(|change| change.summary.clone())
            .collect::<Vec<_>>();
        let diff = render_file_diff(&changes, self);
        let summary = ToolExecutionSummary::file_changes(files.clone(), diff.clone());

        Ok(FileChangePlan {
            changes,
            data,
            files,
            diff,
            summary,
        })
    }

    async fn shell_command(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        let args = match parse_args::<ShellCommandArgs>(call) {
            Ok(args) => args,
            Err(error) => return ToolExecution::error(error),
        };
        if args.command.trim().is_empty() {
            return ToolExecution::error("command must not be empty");
        }
        let timeout_secs = args
            .timeout_secs
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS)
            .min(MAX_SHELL_TIMEOUT_SECS);
        if timeout_secs == 0 {
            return ToolExecution::error("timeout_secs must be at least 1");
        }

        let evaluator = match self.evaluator() {
            Ok(evaluator) => evaluator,
            Err(error) => return ToolExecution::error(error),
        };

        match evaluator.shell_command_decision(&call.id, &args.command, timeout_secs) {
            PermissionDecision::Allow => complete_shell_result(
                run_shell_command(
                    evaluator.root(),
                    &args.command,
                    Duration::from_secs(timeout_secs),
                    cancellation,
                )
                .await,
            ),
            PermissionDecision::Deny(error) => ToolExecution::error(error),
            PermissionDecision::Prompt(request) => match approval {
                None => ToolExecution::ApprovalRequired(request),
                Some((decision, _)) if decision.request_id != request.id => {
                    ToolExecution::error(format!(
                        "approval decision {} does not match required approval {}",
                        decision.request_id, request.id
                    ))
                }
                Some((decision, _)) if !decision.approved => {
                    ToolExecution::error("shell command approval denied")
                }
                Some(_) => complete_shell_result(
                    run_shell_command(
                        evaluator.root(),
                        &args.command,
                        Duration::from_secs(timeout_secs),
                        cancellation,
                    )
                    .await,
                ),
            },
        }
    }

    fn resolve_existing_path(&self, input: &str) -> Result<PathBuf, String> {
        self.evaluator()?.resolve_existing_path(input)
    }

    fn resolve_write_path(&self, input: &str) -> Result<PathBuf, String> {
        self.evaluator()?.resolve_write_path(input)
    }

    fn plan_patch_changes(
        &self,
        operations: Vec<ParsedPatchOperation>,
    ) -> Result<Vec<StagedPatchChange>, String> {
        let mut paths = HashSet::new();
        let mut changes = Vec::with_capacity(operations.len());

        for operation in operations {
            let path = self.resolve_write_path(operation.path())?;
            if !paths.insert(path.clone()) {
                return Err(format!(
                    "patch modifies {} more than once",
                    self.display_path(&path)
                ));
            }

            let change = match operation {
                ParsedPatchOperation::Add { path: _, content } => {
                    match fs::metadata(&path) {
                        Ok(_) => {
                            return Err(format!(
                                "{} already exists; add file cannot overwrite it",
                                self.display_path(&path)
                            ));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(format!(
                                "failed to inspect {}: {err}",
                                self.display_path(&path)
                            ));
                        }
                    }
                    let summary = FileChangeSummary {
                        path: self.display_path(&path),
                        operation: FileChangeOperation::Add,
                        replacements: 0,
                        created: true,
                        overwritten: false,
                        deleted: false,
                    };
                    StagedPatchChange::write(
                        path.clone(),
                        PatchOperationKind::Add,
                        content.clone(),
                        None,
                        summary,
                        None,
                        Some(content),
                    )
                }
                ParsedPatchOperation::Update { path: _, hunks } => {
                    let metadata = fs::metadata(&path).map_err(|err| {
                        format!("failed to inspect {}: {err}", self.display_path(&path))
                    })?;
                    if !metadata.is_file() {
                        return Err(format!("{} is not a file", self.display_path(&path)));
                    }

                    let original = fs::read_to_string(&path).map_err(|err| {
                        format!(
                            "failed to read {} as UTF-8 text: {err}",
                            self.display_path(&path)
                        )
                    })?;
                    let mut updated = original.clone();
                    let mut replacements = 0;
                    for hunk in hunks {
                        let matches = updated.matches(&hunk.old_text).count();
                        if matches != 1 {
                            return Err(format!(
                                "patch hunk for {} must match exactly once; found {matches}",
                                self.display_path(&path)
                            ));
                        }
                        updated = updated.replacen(&hunk.old_text, &hunk.new_text, 1);
                        replacements += 1;
                    }
                    if updated == original {
                        return Err(format!(
                            "patch update for {} did not change file content",
                            self.display_path(&path)
                        ));
                    }

                    StagedPatchChange::write(
                        path.clone(),
                        PatchOperationKind::Update,
                        updated.clone(),
                        Some(metadata.permissions()),
                        FileChangeSummary {
                            path: self.display_path(&path),
                            operation: FileChangeOperation::Update,
                            replacements,
                            created: false,
                            overwritten: true,
                            deleted: false,
                        },
                        Some(original),
                        Some(updated),
                    )
                }
                ParsedPatchOperation::Delete { path: _ } => {
                    let metadata = fs::metadata(&path).map_err(|err| {
                        format!("failed to inspect {}: {err}", self.display_path(&path))
                    })?;
                    if !metadata.is_file() {
                        return Err(format!("{} is not a file", self.display_path(&path)));
                    }
                    let original = fs::read_to_string(&path).map_err(|err| {
                        format!(
                            "failed to read {} as UTF-8 text: {err}",
                            self.display_path(&path)
                        )
                    })?;

                    StagedPatchChange::delete(
                        path.clone(),
                        FileChangeSummary {
                            path: self.display_path(&path),
                            operation: FileChangeOperation::Delete,
                            replacements: 0,
                            created: false,
                            overwritten: false,
                            deleted: true,
                        },
                        Some(original),
                        None,
                    )
                }
            };
            changes.push(change);
        }

        Ok(changes)
    }

    pub(crate) fn display_path(&self, path: &Path) -> String {
        self.evaluator()
            .map(|evaluator| evaluator.display_path(path))
            .unwrap_or_else(|_| path.display().to_string())
    }

    fn evaluator(&self) -> Result<&PermissionEvaluator, String> {
        Ok(&self.evaluator)
    }

    fn path_allowed(&self, path: &Path) -> Result<bool, String> {
        Ok(self.evaluator()?.allows_read_path(path))
    }

    fn collect_entries(
        &self,
        dir: &Path,
        recursive: bool,
        max_entries: usize,
        entries: &mut Vec<Value>,
        truncated: &mut bool,
    ) -> Result<(), String> {
        let mut dir_entries = fs::read_dir(dir)
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?;
        dir_entries.sort_by_key(|entry| entry.file_name());

        for entry in dir_entries {
            if should_skip_entry(&entry.path()) {
                continue;
            }
            if entries.len() >= max_entries {
                *truncated = true;
                return Ok(());
            }

            let path = entry
                .path()
                .canonicalize()
                .map_err(|err| format!("failed to resolve listed path: {err}"))?;
            if !self.path_allowed(&path)? {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
            let kind = if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(json!({
                "path": self.display_path(&path),
                "kind": kind,
            }));

            if recursive && file_type.is_dir() {
                self.collect_entries(&path, recursive, max_entries, entries, truncated)?;
                if *truncated {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn collect_search_files(&self, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut dir_entries = fs::read_dir(dir)
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?;
        dir_entries.sort_by_key(|entry| entry.file_name());

        for entry in dir_entries {
            if should_skip_entry(&entry.path()) {
                continue;
            }
            let path = entry
                .path()
                .canonicalize()
                .map_err(|err| format!("failed to resolve search path: {err}"))?;
            if !self.path_allowed(&path)? {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
            if file_type.is_dir() {
                self.collect_search_files(&path, files)?;
            } else if file_type.is_file() {
                files.push(path);
            }
        }

        Ok(())
    }

    fn search_file(
        &self,
        path: &Path,
        options: &SearchOptions<'_>,
        fail_on_read_error: bool,
        output: &mut SearchOutput,
    ) -> Result<(), String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if fail_on_read_error => {
                return Err(format!(
                    "failed to read {} as UTF-8 text: {err}",
                    self.display_path(path)
                ));
            }
            Err(_) => return Ok(()),
        };
        let needle = if options.case_sensitive {
            options.query.to_string()
        } else {
            options.query.to_lowercase()
        };

        for (index, line) in content.lines().enumerate() {
            let haystack = if options.case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };
            if haystack.contains(&needle)
                && !output.push_match(self.display_path(path), index + 1, line.to_string())
            {
                return Ok(());
            }
        }

        Ok(())
    }
}

pub(crate) fn built_in_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file from the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root."},
                    "start_line": {"type": "integer", "minimum": 1, "description": format!("First line to return (1-based). Defaults to 1. Combine with max_lines to page through large files; the returned output reports the file's total line count.")},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES, "description": format!("Maximum number of lines to return (1..={MAX_READ_LINES}). Defaults to {DEFAULT_READ_LINES}.")}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "list_files",
            "List files and directories under the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list, relative to the workspace root. Defaults to the workspace root."},
                    "recursive": {"type": "boolean", "description": "List entries recursively. Defaults to false (only the directory's immediate children)."},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_ENTRIES, "description": format!("Maximum number of entries to return (1..={MAX_LIST_ENTRIES}). Defaults to {DEFAULT_LIST_ENTRIES}; the output notes when the listing was truncated.")}
                },
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "search_text",
            "Search workspace text files for a literal string.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Literal string to search for (fixed string, not a regular expression)."},
                    "path": {"type": "string", "description": "File or directory to search, relative to the workspace root. Defaults to the whole workspace."},
                    "case_sensitive": {"type": "boolean", "description": "Match case sensitively. Defaults to false."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS, "description": format!("Maximum number of matching lines to return (1..={MAX_SEARCH_RESULTS}). Defaults to {DEFAULT_SEARCH_RESULTS}.")}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "edit_file",
            "Edit a UTF-8 text file by replacing text that matches exactly once. Prefer this for single-point, exact replacements in an existing file; use apply_patch to create or delete files or to change several files at once.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root."},
                    "old_text": {"type": "string", "minLength": 1, "description": "Exact text to replace. It must occur exactly once in the file; include more surrounding context when a shorter snippet is ambiguous."},
                    "new_text": {"type": "string", "description": "Replacement text. May be empty to delete the matched text."}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "write_file",
            "Create or overwrite a UTF-8 text file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root. The parent directory must already exist."},
                    "content": {"type": "string", "description": "Full content to write into the file."},
                    "overwrite": {"type": "boolean", "description": "Allow replacing an existing file. Defaults to false; the call fails when the file already exists and overwrite is not set."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "apply_patch",
            "Apply a patch to add, update, or delete files. Prefer this for new files, deletions, and changes spanning multiple files; use edit_file for single-point, exact replacements in one existing file.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Patch text to apply. The patch must start with *** Begin Patch, then contain one or more *** Add File / *** Update File / *** Delete File sections, and end with *** End Patch. The whole patch is validated before any file is touched."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "shell_command",
            "Run a shell command in the workspace root with a timeout.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run with the workspace root as the working directory."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_SECS, "description": format!("Timeout in seconds (1..={MAX_SHELL_TIMEOUT_SECS}). Defaults to {DEFAULT_SHELL_TIMEOUT_SECS}.")}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
    ]
}

pub(crate) fn known_tool_parameters(name: &str) -> Option<Value> {
    built_in_definitions()
        .into_iter()
        .chain(std::iter::once(delegate_task_definition()))
        .chain(subagent_lifecycle_definitions())
        .chain(std::iter::once(web_fetch::web_fetch_definition()))
        .find(|definition| definition.function.name == name)
        .map(|definition| definition.function.parameters)
}

pub(crate) const MAX_ERROR_MESSAGE_CHARS: usize = 1_024;

/// 参数解析失败时回显合法输入：required 字段列表 + 属性名:类型 紧凑摘要，整体截断。
pub(crate) fn invalid_arguments_message(
    tool_name: &str,
    error: &serde_json::Error,
    parameters: Option<&Value>,
) -> String {
    let mut message = format!("invalid arguments for tool {tool_name}: {error}");
    if let Some(parameters) = parameters {
        let summary = summarize_parameters_schema(parameters);
        if !summary.is_empty() {
            message.push_str(". Tool schema: ");
            message.push_str(&summary);
        }
    }
    if message.chars().count() > MAX_ERROR_MESSAGE_CHARS {
        let truncated: String = message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect();
        message = format!("{truncated}...");
    }
    message
}

fn summarize_parameters_schema(parameters: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(required) = parameters.get("required").and_then(Value::as_array) {
        let fields = required
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("required: [{fields}]"));
    } else if let Some(variants) = parameters.get("oneOf").and_then(Value::as_array) {
        let branches = variants
            .iter()
            .filter_map(|variant| variant.get("required")?.as_array())
            .map(|required| {
                format!(
                    "[{}]",
                    required
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>();
        if !branches.is_empty() {
            parts.push(format!(
                "required (exactly one of): {}",
                branches.join(" | ")
            ));
        }
    }
    if let Some(properties) = parameters.get("properties").and_then(Value::as_object) {
        let props = properties
            .iter()
            .map(|(name, schema)| format!("{name}: {}", schema_type_label(schema)))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("properties: {{{props}}}"));
    }
    parts.join("; ")
}

fn schema_type_label(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => "any".to_string(),
    }
}

pub(crate) fn truncate_chars(text: String, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = chars.clone().nth(max_chars).is_some();
    if !truncated {
        return (text, false);
    }
    (chars.by_ref().take(max_chars).collect(), true)
}

fn tool_ok(data: Value) -> ToolResult {
    tool_ok_inner(data, None)
}

pub(crate) fn tool_ok_with_summary(data: Value, summary: ToolExecutionSummary) -> ToolResult {
    tool_ok_inner(data, Some(summary))
}

fn tool_ok_inner(data: Value, summary: Option<ToolExecutionSummary>) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": true,
        "data": data,
    }))
    .expect("tool result JSON must serialize");
    ToolResult {
        ok: true,
        content,
        error: None,
        summary,
    }
}

fn tool_error(error: String) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": false,
        "error": error,
    }))
    .expect("tool error JSON must serialize");
    ToolResult {
        ok: false,
        error: Some(error.clone()),
        content,
        summary: Some(ToolExecutionSummary::error(error)),
    }
}
