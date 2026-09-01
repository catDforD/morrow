use super::*;

#[derive(Debug, Error)]
pub(crate) enum CliError {
    #[error(transparent)]
    Config(#[from] agent_config::ConfigError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Runtime(#[from] agent_runtime::RuntimeError),
    #[error(transparent)]
    SessionStore(#[from] agent_runtime::SessionStoreError),
    #[error(transparent)]
    SubagentStore(#[from] agent_runtime::SubagentStoreError),
    #[error(transparent)]
    Server(#[from] agent_server::ServerError),
    #[error(transparent)]
    SubagentSettings(#[from] agent_server::SubagentRegistryError),
    #[error(transparent)]
    Hooks(#[from] agent_hooks::HookError),
    #[error("agent run failed: {0}")]
    AgentRun(String),
    #[error("--session and --thread cannot be used together")]
    ConflictingSessionArgs,
    #[error("--jsonl requires a prompt and cannot be used in interactive mode")]
    JsonlRequiresPrompt,
    #[error("--jsonl cannot be used with session commands")]
    JsonlUnsupportedForSessionCommand,
    #[error("home directory was not found")]
    HomeDirNotFound,
    #[error("config file already exists: {path}; use --force to overwrite it")]
    ConfigExists { path: PathBuf },
    #[error("API key must not be empty")]
    EmptyApiKey,
    #[error("failed to create config directory {path}: {source}")]
    ConfigCreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("output file already exists: {path}")]
    OutputExists { path: PathBuf },
    #[error("failed to write output file {path}: {source}")]
    OutputWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize JSONL event: {0}")]
    JsonlSerialize(#[source] serde_json::Error),
    #[error("failed to read stdin: {0}")]
    Stdin(#[source] io::Error),
    #[error("failed to write stderr: {0}")]
    Stderr(#[source] io::Error),
    #[error("failed to write stdout: {0}")]
    Stdout(#[source] io::Error),
}

pub(crate) async fn run() -> Result<(), CliError> {
    let args = Args::parse();
    let session_name = resolve_session_name(&args)?;
    let workspace_root = agent_runtime::detect_workspace_root()?;

    if let Some(command) = args.command.as_ref() {
        if args.jsonl {
            return Err(CliError::JsonlUnsupportedForSessionCommand);
        }
        if !matches!(command, CliCommand::Server { .. }) {
            let mut stdout = io::stdout().lock();
            return handle_cli_command(command, &session_name, &workspace_root, &mut stdout);
        }
    }

    let prompt = args.prompt.join(" ");
    validate_jsonl_prompt(&args, &prompt)?;

    if let Some(CliCommand::Server {
        host,
        port,
        no_auth,
        permission_ceiling,
    }) = args.command.as_ref()
    {
        let loaded = load_server_config(args.config.as_deref())?;
        let home = dirs::home_dir().ok_or(CliError::HomeDirNotFound)?;
        let mut options = agent_server::server_options_from_loaded_config(
            *host,
            *port,
            workspace_root,
            &home,
            loaded,
            session_name,
        )?;
        if let Some(ceiling) = permission_ceiling {
            options.permission_ceiling = *ceiling;
        }
        print_startup_diagnostics(&options.config_diagnostics);
        eprintln!("morrow server listening on http://{host}:{port}");
        let access_policy = if *no_auth {
            eprintln!(
                "WARNING: server authentication is disabled (--no-auth); any local process can drive this agent"
            );
            agent_server::ServerAccessPolicy::browser(None)
        } else {
            let token = agent_server::generate_browser_token()?;
            eprintln!("open this URL to sign in: http://{host}:{port}/?bootstrap={token}");
            agent_server::ServerAccessPolicy::browser(Some(token))
        };
        agent_server::serve(options, access_policy).await?;
        return Ok(());
    }

    let reset_session = args.reset_session || args.reset_thread;
    let home = dirs::home_dir().ok_or(CliError::HomeDirNotFound)?;
    let subagent_store_path = home.join(".morrow").join("subagents.json");
    let loaded = load_config(args.config.as_deref())?;
    // 冷启动预热：打印 AGENTS.md 诊断并填充缓存；之后每个 turn 经缓存重读，
    // 运行中的修改在下一个 turn 生效。turn context 只携带配置层 base prompt。
    let workspace_instructions = WorkspaceInstructionsCache::new(&workspace_root);
    print_startup_diagnostics(&workspace_instructions.prewarm());
    let system_prompt = loaded.config.agent.system_prompt.clone();
    let model_limits = loaded.config.model.context_limits();
    let model_invocation = config_model_invocation(&loaded.config.model.model);
    let permissions =
        effective_permissions(loaded.config.permissions, args.permission, args.allow_shell);
    let auto_approve_workspace_writes = !loaded.config.workspace_write_require_approval;
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: loaded.config.model.base_url,
        model: loaded.config.model.model,
        api_key: loaded.api_key,
        timeout: Duration::from_secs(loaded.config.model.timeout_secs),
        max_retries: loaded
            .config
            .model
            .max_retries
            .unwrap_or(DEFAULT_MAX_RETRIES),
    })?;
    let session_scope_root = workspace_root.clone();
    let session_store = SessionStore::for_workspace(&session_scope_root, &session_name)?;
    if reset_session {
        session_store.reset()?;
        SubagentSessionStore::for_workspace(&session_scope_root, &session_name)?.reset()?;
    }
    let session_handle =
        SessionHandle::open(session_store.clone(), session_name.clone(), permissions)?;
    let mcp_cache = McpToolCache::new();

    if prompt.trim().is_empty() {
        let mut permissions = permissions;
        run_repl(
            ReplContext {
                client: &client,
                model: &model_invocation,
                system_prompt: &system_prompt,
                workspace_instructions: &workspace_instructions,
                context_config: loaded.config.context,
                model_limits,
                session_handle: &session_handle,
                session_name: &session_name,
                workspace_root: &workspace_root,
                session_scope_root: &session_scope_root,
                config_path: &loaded.path,
                mcp_servers: &loaded.config.mcp_servers,
                mcp_cache: &mcp_cache,
                tools: &loaded.config.tools,
                auto_approve_workspace_writes,
                subagent_store_path: &subagent_store_path,
            },
            &mut permissions,
        )
        .await?;
        return Ok(());
    }

    let mut stdout = io::stdout().lock();
    let subagent_identities = agent_server::load_subagent_identities(&subagent_store_path)?;
    let projection = session_handle.projection().await;
    let outcome = run_persisted_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: &model_invocation,
            subagent_identities: &subagent_identities,
            system_prompt: &system_prompt,
            workspace_instructions: Some(&workspace_instructions),
            context_config: loaded.config.context,
            model_limits,
            workspace_root: &workspace_root,
            permissions,
            mcp_servers: &loaded.config.mcp_servers,
            mcp_cache: &mcp_cache,
            tools: &loaded.config.tools,
            auto_approve_workspace_writes,
            interactive_approvals: io::stdin().is_terminal(),
            output: if args.jsonl {
                OutputMode::Jsonl {
                    session_name: &session_name,
                    turn_index: projection.turns.len(),
                }
            } else {
                OutputMode::Human
            },
        },
        &session_handle,
        &prompt,
        &mut stdout,
    )
    .await?;

    if let Some(error) = outcome.error {
        return Err(CliError::AgentRun(error));
    }

    Ok(())
}

fn print_startup_diagnostics(diagnostics: &[String]) {
    for diagnostic in diagnostics {
        eprintln!("warning: {diagnostic}");
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RunAgentTurnContext<'a> {
    pub(crate) client: &'a OpenAiCompatClient,
    pub(crate) model: &'a ModelInvocation,
    pub(crate) subagent_identities: &'a [SubagentIdentity],
    pub(crate) system_prompt: &'a str,
    pub(crate) workspace_instructions: Option<&'a WorkspaceInstructionsCache>,
    pub(crate) context_config: ContextConfig,
    pub(crate) model_limits: ModelContextLimits,
    pub(crate) workspace_root: &'a Path,
    pub(crate) permissions: PermissionProfile,
    pub(crate) mcp_servers: &'a [McpServerConfig],
    pub(crate) mcp_cache: &'a McpToolCache,
    pub(crate) tools: &'a ToolsConfig,
    pub(crate) auto_approve_workspace_writes: bool,
    pub(crate) interactive_approvals: bool,
    pub(crate) output: OutputMode<'a>,
}

pub(crate) async fn run_persisted_agent_turn(
    context: RunAgentTurnContext<'_>,
    session_handle: &SessionHandle,
    prompt: &str,
    stdout: &mut dyn Write,
) -> Result<RunAgentTurnOutcome, CliError> {
    let turn_index = match context.output {
        OutputMode::Human => session_handle.projection().await.turns.len(),
        OutputMode::Jsonl { turn_index, .. } => turn_index,
    };
    let session_name = match context.output {
        OutputMode::Human => session_handle.session_name(),
        OutputMode::Jsonl { session_name, .. } => session_name,
    };
    let mut handler = CliTurnHandler::new(context, stdout);
    let hooks = HookManager::for_workspace(context.workspace_root)?.load_snapshot()?;
    let middleware = hooks.registry();

    agent_runtime::run_agent_turn_with_session_handle_and_middleware_context(
        agent_runtime::MiddlewareAgentTurnContext::new(
            agent_runtime::RunAgentTurnContext {
                client: context.client,
                model: context.model,
                subagent_identities: context.subagent_identities,
                system_prompt: context.system_prompt,
                context_config: context.context_config,
                model_limits: context.model_limits,
                workspace_root: context.workspace_root,
                workspace_instructions: context.workspace_instructions,
                permissions: context.permissions,
                mcp_servers: context.mcp_servers,
                mcp_cache: context.mcp_cache,
                tools: Some(context.tools),
                auto_approve_workspace_writes: context.auto_approve_workspace_writes,
                session_name,
                turn_index,
            },
            middleware.as_ref(),
            agent_protocol::MiddlewareAgentScope::Main,
        ),
        session_handle,
        prompt,
        &mut handler,
        agent_runtime::CancellationToken::new(),
        None,
    )
    .await
    .map_err(CliError::from)
}

#[cfg(test)]
pub(crate) async fn run_agent_turn(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    stdout: &mut dyn Write,
) -> Result<RunAgentTurnOutcome, CliError> {
    let turn_index = match context.output {
        OutputMode::Human => session.turns.len(),
        OutputMode::Jsonl { turn_index, .. } => turn_index,
    };
    let session_name = match context.output {
        OutputMode::Human => "default",
        OutputMode::Jsonl { session_name, .. } => session_name,
    };
    let mut handler = CliTurnHandler::new(context, stdout);
    agent_runtime::run_agent_turn(
        agent_runtime::RunAgentTurnContext {
            client: context.client,
            model: context.model,
            subagent_identities: context.subagent_identities,
            system_prompt: context.system_prompt,
            context_config: context.context_config,
            model_limits: context.model_limits,
            workspace_root: context.workspace_root,
            workspace_instructions: context.workspace_instructions,
            permissions: context.permissions,
            mcp_servers: context.mcp_servers,
            mcp_cache: context.mcp_cache,
            tools: Some(context.tools),
            auto_approve_workspace_writes: context.auto_approve_workspace_writes,
            session_name,
            turn_index,
        },
        session,
        prompt,
        &mut handler,
    )
    .await
    .map_err(CliError::from)
}

struct CliTurnHandler<'a, 'b> {
    context: RunAgentTurnContext<'a>,
    stdout: &'b mut dyn Write,
    wrote_text: bool,
    output_ends_with_newline: bool,
    execution_records: Vec<ExecutionRecord>,
}

impl<'a, 'b> CliTurnHandler<'a, 'b> {
    fn new(context: RunAgentTurnContext<'a>, stdout: &'b mut dyn Write) -> Self {
        Self {
            context,
            stdout,
            wrote_text: false,
            output_ends_with_newline: false,
            execution_records: Vec::new(),
        }
    }
}

impl TurnEventHandler for CliTurnHandler<'_, '_> {
    fn on_event(
        &mut self,
        envelope: &AgentEventEnvelope,
    ) -> Result<(), agent_runtime::RuntimeError> {
        if matches!(self.context.output, OutputMode::Jsonl { .. }) {
            write_jsonl_event(self.stdout, envelope)
                .map_err(agent_runtime::RuntimeError::event_handler)?;
        }

        match &envelope.event {
            AgentEvent::TurnStarted => {}
            AgentEvent::ModelCallStarted => {}
            AgentEvent::MiddlewareStarted(invocation) => {
                if self.context.output == OutputMode::Human {
                    eprintln!(
                        "middleware {} {:?} started",
                        invocation.middleware_id, invocation.stage
                    );
                }
            }
            AgentEvent::MiddlewareFinished(invocation) => {
                if self.context.output == OutputMode::Human {
                    eprintln!(
                        "middleware {} {:?} {:?}",
                        invocation.middleware_id, invocation.stage, invocation.outcome
                    );
                }
            }
            AgentEvent::Warning(message) => {
                if self.context.output == OutputMode::Human {
                    eprintln!("warning: {message}");
                }
            }
            AgentEvent::ReasoningDelta(_) => {}
            AgentEvent::TextDelta(text) => {
                if self.context.output == OutputMode::Human {
                    self.wrote_text = true;
                    self.output_ends_with_newline = text.ends_with('\n');
                    self.stdout
                        .write_all(text.as_bytes())
                        .map_err(CliError::Stdout)
                        .map_err(agent_runtime::RuntimeError::event_handler)?;
                    self.stdout
                        .flush()
                        .map_err(CliError::Stdout)
                        .map_err(agent_runtime::RuntimeError::event_handler)?;
                }
            }
            AgentEvent::ModelMessageCommitted { .. } | AgentEvent::ToolResultCommitted { .. } => {}
            AgentEvent::AgentMessage(_) => {}
            AgentEvent::SubagentStarted {
                agent_name, task, ..
            } => {
                if self.context.output == OutputMode::Human {
                    eprintln!(
                        "subagent {} started: {}",
                        subagent_name(agent_name.as_deref()),
                        compact_line(task, 120)
                    );
                }
            }
            AgentEvent::SubagentFinished { ok, summary, .. } => {
                if self.context.output == OutputMode::Human {
                    let status = if *ok { "ok" } else { "error" };
                    eprintln!(
                        "subagent {} {status}: {} (model_calls={}, tool_calls={})",
                        subagent_name(summary.agent_name.as_deref()),
                        compact_line(&summary.task, 120),
                        summary.model_calls,
                        summary.tool_calls,
                    );
                }
                self.execution_records.push(ExecutionRecord {
                    name: "delegate_task".to_string(),
                    ok: *ok,
                    summary: Some(ToolExecutionSummary::subagent(summary.clone())),
                });
            }
            AgentEvent::SubagentUpdated(_) => {}
            AgentEvent::ToolCallStarted { name, .. } => {
                if self.context.output == OutputMode::Human {
                    eprintln!("tool {name} started");
                }
            }
            AgentEvent::ToolCallFinished {
                name, ok, summary, ..
            } => {
                if self.context.output == OutputMode::Human {
                    let status = if *ok { "ok" } else { "error" };
                    eprintln!("tool {name} {status}");
                }
                self.execution_records.push(ExecutionRecord {
                    name: name.clone(),
                    ok: *ok,
                    summary: summary.clone(),
                });
            }
            AgentEvent::ApprovalRequested(_) => {}
            AgentEvent::ApprovalResolved(decision) => {
                if self.context.output == OutputMode::Human {
                    let status = if decision.approved {
                        "approved"
                    } else {
                        "denied"
                    };
                    eprintln!("approval {} {status}", decision.request_id);
                }
            }
            AgentEvent::TurnCompleted => {
                if self.context.output == OutputMode::Human
                    && self.wrote_text
                    && !self.output_ends_with_newline
                {
                    self.stdout
                        .write_all(b"\n")
                        .map_err(CliError::Stdout)
                        .map_err(agent_runtime::RuntimeError::event_handler)?;
                    self.stdout
                        .flush()
                        .map_err(CliError::Stdout)
                        .map_err(agent_runtime::RuntimeError::event_handler)?;
                }
                if self.context.output == OutputMode::Human {
                    print_execution_summary(&self.execution_records);
                }
            }
            AgentEvent::Error(_) => {}
        }

        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, agent_runtime::RuntimeError>> {
        let result = approval_decision(
            request,
            self.context.permissions,
            self.context.interactive_approvals,
        )
        .map_err(agent_runtime::RuntimeError::event_handler);
        futures_util::future::ready(result).boxed()
    }
}

pub(crate) fn resolve_session_name(args: &Args) -> Result<String, CliError> {
    match (&args.session, &args.thread) {
        (Some(_), Some(_)) => Err(CliError::ConflictingSessionArgs),
        (Some(session), None) => Ok(session.clone()),
        (None, Some(thread)) => Ok(thread.clone()),
        (None, None) => Ok("default".to_string()),
    }
}

pub(crate) fn validate_jsonl_prompt(args: &Args, prompt: &str) -> Result<(), CliError> {
    if args.jsonl && prompt.trim().is_empty() {
        return Err(CliError::JsonlRequiresPrompt);
    }
    Ok(())
}

fn handle_cli_command(
    command: &CliCommand,
    default_session_name: &str,
    workspace_root: &Path,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    match command {
        CliCommand::Init { force, template } => handle_init_command(*force, *template, stdout),
        CliCommand::Server { .. } => Ok(()),
        CliCommand::Session { command } => {
            handle_session_command(command, default_session_name, workspace_root, stdout)
        }
        CliCommand::Hooks { command } => handle_hooks_command(command, workspace_root, stdout),
    }
}

pub(crate) fn config_model_invocation(model_name: &str) -> ModelInvocation {
    ModelInvocation {
        provider_id: CONFIG_PROVIDER_ID.to_string(),
        provider_name: CONFIG_PROVIDER_NAME.to_string(),
        model_id: model_name.to_string(),
        model_name: model_name.to_string(),
        reasoning: ReasoningLevel::Off,
    }
}

pub(crate) fn subagent_name(agent_name: Option<&str>) -> &str {
    agent_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Subagent")
}

pub(crate) fn effective_permissions(
    base: PermissionProfile,
    mode_override: Option<PermissionMode>,
    allow_shell: bool,
) -> PermissionProfile {
    let mut permissions = mode_override
        .map(PermissionProfile::for_mode)
        .unwrap_or(base);
    if allow_shell {
        permissions.shell = ShellPolicy::Allow;
    }
    permissions
}

pub(crate) fn permission_summary(permissions: PermissionProfile) -> String {
    format!(
        "mode={}, shell={}",
        permissions.mode.as_str(),
        permissions.shell.as_str()
    )
}

pub(crate) fn context_summary(context: ContextConfig) -> String {
    format!(
        "auto_compact={}, auto_compact_threshold={}, retain_recent_turns={}, summary_target_tokens={}, compact_max_retries={}",
        context.auto_compact,
        context.auto_compact_threshold,
        context.retain_recent_turns,
        context.summary_target_tokens,
        context.compact_max_retries
    )
}

pub(crate) fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    match value {
        "read-only" | "read_only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" | "workspace_write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" | "danger_full_access" => Ok(PermissionMode::DangerFullAccess),
        _ => Err(format!(
            "invalid permission mode {value:?}; expected read-only, workspace-write, or danger-full-access"
        )),
    }
}
