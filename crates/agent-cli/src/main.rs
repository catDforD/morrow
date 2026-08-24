use agent_config::{
    ContextConfig, McpServerConfig, ModelContextLimits, ToolsConfig, load_config,
    load_server_config,
};
use agent_hooks::{HookManager, HookSettings};
use agent_model::{DEFAULT_MAX_RETRIES, ModelError, OpenAiCompatClient, OpenAiCompatConfig};
#[cfg(test)]
use agent_protocol::Session;
use agent_protocol::{
    AgentEvent, ApprovalAction, ApprovalDecision, ApprovalRequest, FileChangeSummary,
    ModelInvocation, PermissionMode, PermissionProfile, ReasoningLevel, ShellCommandSummary,
    ShellPolicy, SubagentIdentity, ToolExecutionSummary,
};
use agent_runtime::{
    AgentEventEnvelope, CompactionOutcome, McpToolCache, RunAgentTurnOutcome, SessionHandle,
    SessionStore, SubagentSessionStore, TurnEventHandler, load_workspace_instructions,
};
use clap::{Parser, Subcommand};
use futures_util::future::{BoxFuture, FutureExt};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const INIT_CONFIG_MODEL: &str = "gpt-4.1";
const INIT_CONFIG_BASE_URL: &str = "https://api.openai.com/v1";
const INIT_CONFIG_API_KEY_PLACEHOLDER: &str = "replace-with-your-openai-api-key";
const INIT_CONFIG_TIMEOUT_SECS: u64 = 120;
const INIT_CONFIG_CONTEXT_WINDOW_TOKENS: usize = 1_047_576;
const INIT_CONFIG_RESERVED_OUTPUT_TOKENS: usize = 8_192;
const CONFIG_PROVIDER_ID: &str = "current-config";
const CONFIG_PROVIDER_NAME: &str = "默认配置";

#[derive(Debug, Parser)]
#[command(name = "morrow")]
#[command(about = "Minimal OpenAI-compatible agent loop CLI")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    session: Option<String>,

    #[arg(long, help = "Deprecated alias for --session")]
    thread: Option<String>,

    #[arg(long)]
    reset_session: bool,

    #[arg(long, help = "Deprecated alias for --reset-session")]
    reset_thread: bool,

    #[arg(long, value_parser = parse_permission_mode)]
    permission: Option<PermissionMode>,

    #[arg(long)]
    allow_shell: bool,

    #[arg(long)]
    jsonl: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,

    #[arg(value_name = "PROMPT", num_args = 0..)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        template: bool,
    },
    Server {
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        #[arg(long, default_value_t = 3000)]
        port: u16,
        #[arg(
            long,
            help = "Disable browser session authentication (local debugging only)"
        )]
        no_auth: bool,
        #[arg(
            long,
            value_parser = parse_permission_mode,
            help = "Cap the permission mode web clients may request per turn"
        )]
        permission_ceiling: Option<PermissionMode>,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
}

#[derive(Debug, Subcommand)]
enum HooksCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Trust,
    Revoke,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    List,
    Show {
        name: Option<String>,
    },
    Delete {
        name: String,
    },
    Rename {
        old: String,
        new: String,
    },
    Export {
        name: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Error)]
enum CliError {
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

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), CliError> {
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
    let workspace_instructions =
        load_workspace_instructions(&workspace_root, &loaded.config.agent.system_prompt);
    print_startup_diagnostics(&workspace_instructions.diagnostics);
    let system_prompt = workspace_instructions.effective_system_prompt;
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

struct ReplContext<'a> {
    client: &'a OpenAiCompatClient,
    model: &'a ModelInvocation,
    system_prompt: &'a str,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    session_handle: &'a SessionHandle,
    session_name: &'a str,
    workspace_root: &'a Path,
    session_scope_root: &'a Path,
    config_path: &'a Path,
    mcp_servers: &'a [McpServerConfig],
    mcp_cache: &'a McpToolCache,
    tools: &'a ToolsConfig,
    auto_approve_workspace_writes: bool,
    subagent_store_path: &'a Path,
}

#[derive(Debug, Clone, Copy)]
struct RunAgentTurnContext<'a> {
    client: &'a OpenAiCompatClient,
    model: &'a ModelInvocation,
    subagent_identities: &'a [SubagentIdentity],
    system_prompt: &'a str,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    workspace_root: &'a Path,
    permissions: PermissionProfile,
    mcp_servers: &'a [McpServerConfig],
    mcp_cache: &'a McpToolCache,
    tools: &'a ToolsConfig,
    auto_approve_workspace_writes: bool,
    interactive_approvals: bool,
    output: OutputMode<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode<'a> {
    Human,
    Jsonl {
        session_name: &'a str,
        turn_index: usize,
    },
}

#[derive(Debug, Clone)]
struct ExecutionRecord {
    name: String,
    ok: bool,
    summary: Option<ToolExecutionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputLine {
    text: String,
    had_invalid_utf8: bool,
}

async fn run_repl(
    context: ReplContext<'_>,
    permissions: &mut PermissionProfile,
) -> Result<(), CliError> {
    eprintln!("morrow interactive mode. Type /exit to quit.");

    loop {
        eprint!("morrow> ");
        io::stderr().flush().map_err(CliError::Stderr)?;

        let Some(input) = read_stdin_line()? else {
            break;
        };
        warn_if_lossy_input(&input);

        let input = input.text.trim();
        if input.is_empty() {
            continue;
        }

        if input.starts_with('/') {
            if handle_repl_command(input, &context, permissions).await? {
                break;
            }
            continue;
        }

        let mut stdout = io::stdout().lock();
        let subagent_identities =
            agent_server::load_subagent_identities(context.subagent_store_path)?;
        let outcome = run_persisted_agent_turn(
            RunAgentTurnContext {
                client: context.client,
                model: context.model,
                subagent_identities: &subagent_identities,
                system_prompt: context.system_prompt,
                context_config: context.context_config,
                model_limits: context.model_limits,
                workspace_root: context.workspace_root,
                permissions: *permissions,
                mcp_servers: context.mcp_servers,
                mcp_cache: context.mcp_cache,
                tools: context.tools,
                auto_approve_workspace_writes: context.auto_approve_workspace_writes,
                interactive_approvals: io::stdin().is_terminal(),
                output: OutputMode::Human,
            },
            context.session_handle,
            input,
            &mut stdout,
        )
        .await?;

        if let Some(error) = outcome.error {
            return Err(CliError::AgentRun(error));
        }
    }

    Ok(())
}

async fn handle_repl_command(
    input: &str,
    context: &ReplContext<'_>,
    permissions: &mut PermissionProfile,
) -> Result<bool, CliError> {
    let mut parts = input.split_whitespace();
    let command = parts.next().unwrap_or_default();

    match command {
        "/exit" | "/quit" => Ok(true),
        "/status" => {
            let projection = context.session_handle.projection().await;
            eprintln!("session: {}", context.session_name);
            eprintln!("turns: {}", projection.turns.len());
            eprintln!("active messages: {}", projection.context.messages.len());
            eprintln!(
                "summary: {}",
                if projection.context.summary.is_some() {
                    "yes"
                } else {
                    "no"
                }
            );
            eprintln!("workspace: {}", context.workspace_root.display());
            eprintln!("config: {}", context.config_path.display());
            eprintln!("context: {}", context_summary(context.context_config));
            eprintln!("permissions: {}", permission_summary(*permissions));
            Ok(false)
        }
        "/reset" => {
            context.session_handle.hard_reset().await?;
            SubagentSessionStore::for_workspace(context.session_scope_root, context.session_name)?
                .reset()?;
            eprintln!("session reset");
            Ok(false)
        }
        "/compact" => {
            let projection = context.session_handle.projection().await;
            let mut session = agent_runtime::projection_to_legacy_session(&projection);
            let hooks = HookManager::for_workspace(context.workspace_root)?.load_snapshot()?;
            let compaction = agent_runtime::compact_session_with_middleware_audit(
                context.client,
                &mut session,
                context.context_config,
                agent_runtime::MiddlewareExecutionContext {
                    invocation_id: None,
                    session: context.session_name.to_string(),
                    workspace_root: context.workspace_root.to_path_buf(),
                    turn_index: projection.turns.len(),
                    operation_id: None,
                    turn_id: None,
                    model: context.model.clone(),
                    permissions: *permissions,
                    agent_scope: agent_protocol::MiddlewareAgentScope::Main,
                    cancellation: agent_runtime::CancellationToken::new(),
                },
                hooks.registry().as_ref(),
            )
            .await;
            let events = match &compaction {
                Ok(compaction) => &compaction.events,
                Err(failure) => &failure.events,
            };
            for event in events {
                if let AgentEvent::MiddlewareFinished(invocation) = event {
                    context
                        .session_handle
                        .commit_fact(
                            None,
                            None,
                            agent_protocol::SessionFact::MiddlewareFinished {
                                invocation: invocation.clone(),
                            },
                        )
                        .await?;
                    eprintln!(
                        "middleware {} {:?}: {:?}",
                        invocation.middleware_id, invocation.stage, invocation.outcome
                    );
                }
            }
            let compaction = compaction.map_err(|failure| failure.error)?;
            match compaction.outcome {
                CompactionOutcome::Changed => {
                    let covered = projection
                        .turns
                        .get(session.context.summarized_turns.saturating_sub(1))
                        .map(|turn| turn.id.clone())
                        .ok_or_else(|| {
                            CliError::AgentRun(
                                "compaction did not resolve a covered turn".to_string(),
                            )
                        })?;
                    context
                        .session_handle
                        .commit_fact(
                            None,
                            None,
                            agent_protocol::SessionFact::ContextCompacted {
                                summary: session
                                    .context
                                    .summary
                                    .clone()
                                    .expect("changed compaction has summary"),
                                covered_through_turn_id: covered,
                            },
                        )
                        .await?;
                    eprintln!("session compacted");
                }
                CompactionOutcome::Noop => {
                    eprintln!("no compactable session history");
                }
            }
            Ok(false)
        }
        "/permissions" => {
            let Some(mode) = parts.next() else {
                eprintln!("permissions: {}", permission_summary(*permissions));
                return Ok(false);
            };
            match parse_permission_mode(mode) {
                Ok(mode) => {
                    *permissions = PermissionProfile::for_mode(mode);
                    eprintln!("permissions: {}", permission_summary(*permissions));
                }
                Err(error) => eprintln!("{error}"),
            }
            Ok(false)
        }
        _ => {
            eprintln!("unknown command: {command}");
            Ok(false)
        }
    }
}

async fn run_persisted_agent_turn(
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
async fn run_agent_turn(
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

fn resolve_session_name(args: &Args) -> Result<String, CliError> {
    match (&args.session, &args.thread) {
        (Some(_), Some(_)) => Err(CliError::ConflictingSessionArgs),
        (Some(session), None) => Ok(session.clone()),
        (None, Some(thread)) => Ok(thread.clone()),
        (None, None) => Ok("default".to_string()),
    }
}

fn validate_jsonl_prompt(args: &Args, prompt: &str) -> Result<(), CliError> {
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

fn handle_hooks_command(
    command: &HooksCommand,
    workspace_root: &Path,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let manager = HookManager::for_workspace(workspace_root)?;
    handle_hooks_command_with_manager(command, &manager, stdout)
}

fn handle_hooks_command_with_manager(
    command: &HooksCommand,
    manager: &HookManager,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let settings = match command {
        HooksCommand::List { json } => {
            let settings = manager.settings()?;
            if *json {
                serde_json::to_writer_pretty(&mut *stdout, &settings)
                    .map_err(CliError::JsonlSerialize)?;
                stdout.write_all(b"\n").map_err(CliError::Stdout)?;
                return stdout.flush().map_err(CliError::Stdout);
            }
            settings
        }
        HooksCommand::Trust => manager.trust_project()?,
        HooksCommand::Revoke => manager.revoke_project()?,
    };
    write_hook_settings(stdout, &settings)?;
    stdout.flush().map_err(CliError::Stdout)
}

fn write_hook_settings(stdout: &mut dyn Write, settings: &HookSettings) -> Result<(), CliError> {
    let project_status = match (
        settings.project_fingerprint.as_ref(),
        settings.project_trusted,
    ) {
        (None, _) => "not configured",
        (Some(_), true) => "trusted",
        (Some(_), false) => "not trusted",
    };
    writeln!(
        stdout,
        "project hooks: {}{}",
        project_status,
        settings
            .project_fingerprint
            .as_ref()
            .map(|fingerprint| format!(" ({fingerprint})"))
            .unwrap_or_default()
    )
    .map_err(CliError::Stdout)?;
    for hook in &settings.hooks {
        writeln!(
            stdout,
            "{}\t{}\t{:?}\t{}\t{}",
            hook.id,
            hook.event.as_str(),
            hook.source,
            if hook.active { "active" } else { "disabled" },
            hook.command.join(" ")
        )
        .map_err(CliError::Stdout)?;
    }
    for diagnostic in &settings.diagnostics {
        writeln!(stdout, "warning: {diagnostic}").map_err(CliError::Stdout)?;
    }
    Ok(())
}

fn handle_init_command(
    force: bool,
    template: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let path = default_config_path()?;
    let api_key = if template {
        INIT_CONFIG_API_KEY_PLACEHOLDER.to_string()
    } else {
        read_init_api_key()?
    };

    write_init_config(&path, &api_key, force)?;
    writeln!(stdout, "wrote config: {}", path.display()).map_err(CliError::Stdout)?;
    if template {
        writeln!(stdout, "edit [model].OPENAI_API_KEY before running morrow")
            .map_err(CliError::Stdout)?;
    } else {
        writeln!(stdout, "try: morrow \"hello\"").map_err(CliError::Stdout)?;
    }
    Ok(())
}

fn default_config_path() -> Result<PathBuf, CliError> {
    let home = dirs::home_dir().ok_or(CliError::HomeDirNotFound)?;
    Ok(default_config_path_for_home(&home))
}

fn default_config_path_for_home(home: &Path) -> PathBuf {
    home.join(".morrow").join("config.toml")
}

fn read_init_api_key() -> Result<String, CliError> {
    eprint!("OpenAI API key: ");
    io::stderr().flush().map_err(CliError::Stderr)?;
    let input = read_stdin_line()?.ok_or_else(|| CliError::EmptyApiKey)?;
    warn_if_lossy_input(&input);
    let api_key = input.text.trim().to_string();
    if api_key.is_empty() {
        return Err(CliError::EmptyApiKey);
    }
    Ok(api_key)
}

fn write_init_config(path: &Path, api_key: &str, force: bool) -> Result<(), CliError> {
    if path.exists() && !force {
        return Err(CliError::ConfigExists {
            path: path.to_path_buf(),
        });
    }
    if api_key.trim().is_empty() {
        return Err(CliError::EmptyApiKey);
    }

    let parent = path.parent().expect("config path must have parent");
    fs::create_dir_all(parent).map_err(|source| CliError::ConfigCreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::write(path, render_init_config(api_key)).map_err(|source| CliError::ConfigWrite {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn render_init_config(api_key: &str) -> String {
    format!(
        r#"[model]
base_url = "{INIT_CONFIG_BASE_URL}"
model = "{INIT_CONFIG_MODEL}"
OPENAI_API_KEY = "{api_key}"
timeout_secs = {INIT_CONFIG_TIMEOUT_SECS}
context_window_tokens = {INIT_CONFIG_CONTEXT_WINDOW_TOKENS}
reserved_output_tokens = {INIT_CONFIG_RESERVED_OUTPUT_TOKENS}

[permissions]
mode = "read_only"
shell = "deny"
"#
    )
}

fn handle_session_command(
    command: &SessionCommand,
    default_session_name: &str,
    workspace_root: &Path,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    match command {
        SessionCommand::List => {
            let store = SessionStore::for_workspace(workspace_root, default_session_name)?;
            let entries = store.list_current_scope()?;
            if entries.is_empty() {
                writeln!(stdout, "no sessions").map_err(CliError::Stdout)?;
            } else {
                writeln!(stdout, "NAME\tTURNS\tACTIVE_MESSAGES\tSUMMARY\tPATH")
                    .map_err(CliError::Stdout)?;
                for entry in entries {
                    writeln!(
                        stdout,
                        "{}\t{}\t{}\t{}\t{}",
                        entry.name,
                        entry.turns,
                        entry.active_messages,
                        if entry.has_summary { "yes" } else { "no" },
                        entry.path.display()
                    )
                    .map_err(CliError::Stdout)?;
                }
            }
        }
        SessionCommand::Show { name } => {
            let name = name.as_deref().unwrap_or(default_session_name);
            let store = SessionStore::for_workspace(workspace_root, name)?;
            let session = store.load_existing()?;
            writeln!(stdout, "name: {name}").map_err(CliError::Stdout)?;
            writeln!(stdout, "path: {}", store.path().display()).map_err(CliError::Stdout)?;
            writeln!(stdout, "turns: {}", session.turns.len()).map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "active_messages: {}",
                session.active_thread.messages.len()
            )
            .map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "summarized_turns: {}",
                session.context.summarized_turns
            )
            .map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "summary: {}",
                if session.context.summary.is_some() {
                    "yes"
                } else {
                    "no"
                }
            )
            .map_err(CliError::Stdout)?;
        }
        SessionCommand::Delete { name } => {
            let store = SessionStore::for_workspace(workspace_root, name)?;
            store.delete()?;
            SubagentSessionStore::for_workspace(workspace_root, name)?.delete_all()?;
            writeln!(stdout, "deleted session: {name}").map_err(CliError::Stdout)?;
        }
        SessionCommand::Rename { old, new } => {
            let store = SessionStore::for_workspace(workspace_root, old)?;
            let subagents = SubagentSessionStore::for_workspace(workspace_root, old)?;
            let renamed_subagents = subagents.rename(new)?;
            let target = match store.rename(new) {
                Ok(target) => target,
                Err(error) => {
                    let _ = renamed_subagents.rename(old);
                    return Err(error.into());
                }
            };
            writeln!(
                stdout,
                "renamed session: {old} -> {new} ({})",
                target.path().display()
            )
            .map_err(CliError::Stdout)?;
        }
        SessionCommand::Export { name, output } => {
            let name = name.as_deref().unwrap_or(default_session_name);
            let store = SessionStore::for_workspace(workspace_root, name)?;
            let bytes = store.export_document_bytes()?;
            if let Some(path) = output {
                if path.exists() {
                    return Err(CliError::OutputExists { path: path.clone() });
                }
                fs::write(path, &bytes).map_err(|source| CliError::OutputWrite {
                    path: path.clone(),
                    source,
                })?;
                eprintln!("exported session: {name} -> {}", path.display());
            } else {
                stdout.write_all(&bytes).map_err(CliError::Stdout)?;
                stdout.write_all(b"\n").map_err(CliError::Stdout)?;
            }
        }
    }

    stdout.flush().map_err(CliError::Stdout)
}

fn write_jsonl_event(
    stdout: &mut dyn Write,
    envelope: &AgentEventEnvelope,
) -> Result<(), CliError> {
    serde_json::to_writer(&mut *stdout, envelope).map_err(CliError::JsonlSerialize)?;
    stdout.write_all(b"\n").map_err(CliError::Stdout)?;
    stdout.flush().map_err(CliError::Stdout)?;
    Ok(())
}

fn approval_decision(
    request: &ApprovalRequest,
    permissions: PermissionProfile,
    interactive: bool,
) -> Result<ApprovalDecision, CliError> {
    print_approval_request(request, permissions);

    if !interactive {
        eprintln!("stdin is not interactive; approval denied by default");
        return Ok(ApprovalDecision::deny(request.id.clone()));
    }

    eprint!("approve this action? [y/N] ");
    io::stderr().flush().map_err(CliError::Stderr)?;

    let input = read_stdin_line()?.unwrap_or_else(|| InputLine {
        text: String::new(),
        had_invalid_utf8: false,
    });
    warn_if_lossy_input(&input);
    let approved = matches!(input.text.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    Ok(if approved {
        ApprovalDecision::approve(request.id.clone())
    } else {
        ApprovalDecision::deny(request.id.clone())
    })
}

fn print_approval_request(request: &ApprovalRequest, permissions: PermissionProfile) {
    eprint!("{}", format_approval_request(request, permissions));
}

fn read_stdin_line() -> Result<Option<InputLine>, CliError> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    read_input_line(&mut stdin).map_err(CliError::Stdin)
}

fn read_input_line(reader: &mut impl BufRead) -> io::Result<Option<InputLine>> {
    let mut bytes = Vec::new();
    let read = reader.read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }

    Ok(Some(match String::from_utf8(bytes) {
        Ok(text) => InputLine {
            text,
            had_invalid_utf8: false,
        },
        Err(error) => InputLine {
            text: String::from_utf8_lossy(&error.into_bytes()).into_owned(),
            had_invalid_utf8: true,
        },
    }))
}

fn warn_if_lossy_input(input: &InputLine) {
    if input.had_invalid_utf8 {
        eprintln!("warning: stdin contained invalid UTF-8; replaced invalid bytes");
    }
}

fn format_approval_request(request: &ApprovalRequest, permissions: PermissionProfile) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "approval required: {}", request.reason);
    match &request.action {
        ApprovalAction::ShellCommand {
            command,
            cwd,
            timeout_secs,
        } => {
            let _ = writeln!(output, "action: shell command");
            let _ = writeln!(output, "command: {command}");
            let _ = writeln!(output, "cwd: {}", cwd.display());
            let _ = writeln!(output, "timeout: {timeout_secs}s");
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(
                output,
                "warning: approving this command may modify files or access the network."
            );
        }
        ApprovalAction::FileChanges { files, diff } => {
            let _ = writeln!(output, "action: file changes");
            append_file_list(&mut output, files);
            let _ = writeln!(output, "diff:");
            output.push_str(diff);
            if !diff.ends_with('\n') {
                output.push('\n');
            }
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(output, "warning: approving this action will modify files.");
        }
        ApprovalAction::McpTool {
            server,
            tool,
            arguments,
        } => {
            let _ = writeln!(output, "action: mcp tool");
            let _ = writeln!(output, "server: {server}");
            let _ = writeln!(output, "tool: {tool}");
            let _ = writeln!(output, "arguments: {arguments}");
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(
                output,
                "warning: approving this action lets the MCP server perform side effects."
            );
        }
    }
    output
}

fn print_execution_summary(records: &[ExecutionRecord]) {
    if let Some(summary) = format_execution_summary(records) {
        eprint!("{summary}");
    }
}

fn format_execution_summary(records: &[ExecutionRecord]) -> Option<String> {
    if records.is_empty() {
        return None;
    }

    let mut output = String::from("execution summary:\n");
    for record in records {
        let status = if record.ok { "ok" } else { "error" };
        let _ = writeln!(output, "- {}: {status}", record.name);
        if let Some(summary) = record.summary.as_ref() {
            if !summary.files.is_empty() {
                append_file_list(&mut output, &summary.files);
                if summary.diff.as_deref().is_some_and(|diff| !diff.is_empty()) {
                    let _ = writeln!(output, "  diff: available");
                }
            }
            if let Some(shell) = summary.shell.as_ref() {
                append_shell_summary(&mut output, shell);
            }
            if let Some(error) = summary.error.as_ref() {
                let _ = writeln!(output, "  error: {error}");
            }
            if let Some(subagent) = summary.subagent.as_ref() {
                let _ = writeln!(
                    output,
                    "  agent: {}",
                    subagent_name(subagent.agent_name.as_deref())
                );
                let _ = writeln!(output, "  task: {}", compact_line(&subagent.task, 160));
                let _ = writeln!(
                    output,
                    "  subagent: model_calls={}, tool_calls={}, truncated={}",
                    subagent.model_calls, subagent.tool_calls, subagent.truncated
                );
                if let Some(error) = subagent.error.as_ref() {
                    let _ = writeln!(output, "  error: {error}");
                }
            }
        }
    }

    Some(output)
}

fn compact_line(value: &str, max_chars: usize) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let mut compact = one_line
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    compact.push('…');
    compact
}

fn config_model_invocation(model_name: &str) -> ModelInvocation {
    ModelInvocation {
        provider_id: CONFIG_PROVIDER_ID.to_string(),
        provider_name: CONFIG_PROVIDER_NAME.to_string(),
        model_id: model_name.to_string(),
        model_name: model_name.to_string(),
        reasoning: ReasoningLevel::Off,
    }
}

fn subagent_name(agent_name: Option<&str>) -> &str {
    agent_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Subagent")
}

fn append_file_list(output: &mut String, files: &[FileChangeSummary]) {
    if files.is_empty() {
        let _ = writeln!(output, "files: none");
        return;
    }

    let _ = writeln!(output, "files:");
    for file in files {
        let _ = writeln!(
            output,
            "- {} ({}, replacements={}, created={}, overwritten={}, deleted={})",
            file.path,
            file.operation.as_str(),
            file.replacements,
            file.created,
            file.overwritten,
            file.deleted
        );
    }
}

fn append_shell_summary(output: &mut String, shell: &ShellCommandSummary) {
    let exit_code = shell
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_string());
    let _ = writeln!(
        output,
        "  shell: exit_code={exit_code}, timed_out={}, stdout_truncated={}, stderr_truncated={}",
        shell.timed_out, shell.stdout_truncated, shell.stderr_truncated
    );
}

fn effective_permissions(
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

fn permission_summary(permissions: PermissionProfile) -> String {
    format!(
        "mode={}, shell={}",
        permissions.mode.as_str(),
        permissions.shell.as_str()
    )
}

fn context_summary(context: ContextConfig) -> String {
    format!(
        "auto_compact={}, auto_compact_threshold={}, retain_recent_turns={}, summary_target_tokens={}, compact_max_retries={}",
        context.auto_compact,
        context.auto_compact_threshold,
        context.retain_recent_turns,
        context.summary_target_tokens,
        context.compact_max_retries
    )
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    match value {
        "read-only" | "read_only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" | "workspace_write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" | "danger_full_access" => Ok(PermissionMode::DangerFullAccess),
        _ => Err(format!(
            "invalid permission mode {value:?}; expected read-only, workspace-write, or danger-full-access"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{FileChangeOperation, Message, Thread, Turn, TurnRecord, TurnStatus};
    use agent_runtime::{compact_session, rebuild_active_thread};
    use serde_json::json;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_model_invocation() -> &'static ModelInvocation {
        static MODEL: OnceLock<ModelInvocation> = OnceLock::new();
        MODEL.get_or_init(|| config_model_invocation("test-model"))
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
            max_retries: 1,
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

    fn unique_cli_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-cli-{name}-{stamp}"));
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
            max_context_tokens: Some(300_000),
        }
    }

    fn model_limits(context_window_tokens: usize) -> ModelContextLimits {
        ModelContextLimits {
            context_window_tokens,
            reserved_output_tokens: 128,
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
            context: agent_protocol::SessionContext::new(),
        };
        rebuild_active_thread(&mut session);
        session
    }

    fn file_summary() -> FileChangeSummary {
        FileChangeSummary {
            path: "note.txt".to_string(),
            operation: FileChangeOperation::Add,
            replacements: 0,
            created: true,
            overwritten: false,
            deleted: false,
        }
    }

    #[test]
    fn read_input_line_accepts_valid_utf8() {
        let mut input = std::io::Cursor::new(vec![0xe4, 0xbd, 0xa0, b'\n']);

        let line = read_input_line(&mut input)
            .expect("read line")
            .expect("line");

        assert_eq!(line.text, "\u{4f60}\n");
        assert!(!line.had_invalid_utf8);
    }

    #[test]
    fn read_input_line_replaces_invalid_utf8() {
        let mut input = std::io::Cursor::new(vec![b'h', 0xff, b'i', b'\n']);

        let line = read_input_line(&mut input)
            .expect("read line")
            .expect("line");

        assert_eq!(line.text, "h\u{fffd}i\n");
        assert!(line.had_invalid_utf8);
    }

    #[test]
    fn read_input_line_returns_none_on_eof() {
        let mut input = std::io::Cursor::new(Vec::new());

        let line = read_input_line(&mut input).expect("read eof");

        assert_eq!(line, None);
    }

    #[test]
    fn parses_permission_modes_for_cli_and_repl() {
        assert_eq!(
            parse_permission_mode("read-only").expect("read-only"),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            parse_permission_mode("workspace_write").expect("workspace_write"),
            PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            parse_permission_mode("danger-full-access").expect("danger-full-access"),
            PermissionMode::DangerFullAccess
        );
        assert!(parse_permission_mode("full").is_err());
    }

    #[test]
    fn resolves_session_cli_args_and_thread_alias() {
        let default_args = Args::try_parse_from(["morrow"]).expect("default args");
        assert_eq!(
            resolve_session_name(&default_args).expect("session"),
            "default"
        );

        let session_args =
            Args::try_parse_from(["morrow", "--session", "work"]).expect("session args");
        assert_eq!(
            resolve_session_name(&session_args).expect("session"),
            "work"
        );

        let thread_args =
            Args::try_parse_from(["morrow", "--thread", "legacy"]).expect("thread args");
        assert_eq!(
            resolve_session_name(&thread_args).expect("session"),
            "legacy"
        );

        let conflicting =
            Args::try_parse_from(["morrow", "--session", "work", "--thread", "legacy"])
                .expect("parse conflicting aliases");
        assert!(matches!(
            resolve_session_name(&conflicting),
            Err(CliError::ConflictingSessionArgs)
        ));
    }

    #[test]
    fn parses_server_flags() {
        let default_args = Args::try_parse_from(["morrow", "server"]).expect("parse server");
        assert!(matches!(
            default_args.command,
            Some(CliCommand::Server {
                no_auth: false,
                permission_ceiling: None,
                ..
            })
        ));

        let args = Args::try_parse_from([
            "morrow",
            "server",
            "--port",
            "3100",
            "--no-auth",
            "--permission-ceiling",
            "workspace-write",
        ])
        .expect("parse server flags");
        assert!(matches!(
            args.command,
            Some(CliCommand::Server {
                port: 3100,
                no_auth: true,
                permission_ceiling: Some(PermissionMode::WorkspaceWrite),
                ..
            })
        ));
    }

    #[test]
    fn parses_session_subcommands() {
        let init_args =
            Args::try_parse_from(["morrow", "init", "--template"]).expect("parse init template");
        assert!(matches!(
            init_args.command,
            Some(CliCommand::Init {
                force: false,
                template: true
            })
        ));

        let force_init_args =
            Args::try_parse_from(["morrow", "init", "--force"]).expect("parse init force");
        assert!(matches!(
            force_init_args.command,
            Some(CliCommand::Init {
                force: true,
                template: false
            })
        ));

        let list_args =
            Args::try_parse_from(["morrow", "session", "list"]).expect("parse session list");
        assert!(matches!(
            list_args.command,
            Some(CliCommand::Session {
                command: SessionCommand::List
            })
        ));

        let export_args = Args::try_parse_from([
            "morrow",
            "--session",
            "work",
            "session",
            "export",
            "--output",
            "session.json",
        ])
        .expect("parse session export");
        assert!(matches!(
            export_args.command,
            Some(CliCommand::Session {
                command: SessionCommand::Export { .. }
            })
        ));
        assert_eq!(resolve_session_name(&export_args).expect("session"), "work");
    }

    #[test]
    fn parses_hooks_subcommands() {
        let list =
            Args::try_parse_from(["morrow", "hooks", "list", "--json"]).expect("parse hooks list");
        assert!(matches!(
            list.command,
            Some(CliCommand::Hooks {
                command: HooksCommand::List { json: true }
            })
        ));
        let trust = Args::try_parse_from(["morrow", "hooks", "trust"]).expect("parse hooks trust");
        assert!(matches!(
            trust.command,
            Some(CliCommand::Hooks {
                command: HooksCommand::Trust
            })
        ));
        let revoke =
            Args::try_parse_from(["morrow", "hooks", "revoke"]).expect("parse hooks revoke");
        assert!(matches!(
            revoke.command,
            Some(CliCommand::Hooks {
                command: HooksCommand::Revoke
            })
        ));
    }

    #[test]
    fn hooks_commands_list_trust_and_revoke_project_configuration() {
        let home = unique_cli_dir("hooks-home");
        let workspace = unique_cli_dir("hooks-workspace");
        let manager = HookManager::new(&home, &workspace);
        fs::create_dir_all(
            manager
                .project_config_path()
                .parent()
                .expect("project hook parent"),
        )
        .expect("create project hook parent");
        fs::write(
            manager.project_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
        )
        .expect("write project hooks");

        let mut json_output = Vec::new();
        handle_hooks_command_with_manager(
            &HooksCommand::List { json: true },
            &manager,
            &mut json_output,
        )
        .expect("list hooks");
        let listed: serde_json::Value = serde_json::from_slice(&json_output).expect("hooks JSON");
        assert_eq!(listed["project_trusted"], false);
        assert_eq!(listed["hooks"][0]["active"], false);

        let mut trust_output = Vec::new();
        handle_hooks_command_with_manager(&HooksCommand::Trust, &manager, &mut trust_output)
            .expect("trust hooks");
        let trust_output = String::from_utf8(trust_output).expect("trust output");
        assert!(trust_output.contains("project hooks: trusted"));
        assert!(trust_output.contains("project\tbefore_prompt\tProject\tactive"));

        let mut revoke_output = Vec::new();
        handle_hooks_command_with_manager(&HooksCommand::Revoke, &manager, &mut revoke_output)
            .expect("revoke hooks");
        assert!(
            String::from_utf8(revoke_output)
                .expect("revoke output")
                .contains("project hooks: not trusted")
        );
    }

    #[test]
    fn init_config_writes_global_config_template() {
        let home = unique_cli_dir("init-home");
        let path = default_config_path_for_home(&home);

        write_init_config(&path, INIT_CONFIG_API_KEY_PLACEHOLDER, false)
            .expect("write init config");

        let content = fs::read_to_string(path).expect("read init config");
        assert!(content.contains(r#"base_url = "https://api.openai.com/v1""#));
        assert!(content.contains(r#"model = "gpt-4.1""#));
        assert!(content.contains(r#"OPENAI_API_KEY = "replace-with-your-openai-api-key""#));
        assert!(content.contains(r#"mode = "read_only""#));
        assert!(content.contains(r#"shell = "deny""#));
    }

    #[test]
    fn init_config_refuses_existing_file_unless_forced() {
        let home = unique_cli_dir("init-force-home");
        let path = default_config_path_for_home(&home);
        write_init_config(&path, "first-key", false).expect("write first config");

        let err = write_init_config(&path, "second-key", false).expect_err("must not overwrite");

        assert!(matches!(err, CliError::ConfigExists { .. }));
        assert!(
            fs::read_to_string(&path)
                .expect("read preserved config")
                .contains("first-key")
        );

        write_init_config(&path, "second-key", true).expect("force overwrite");
        assert!(
            fs::read_to_string(path)
                .expect("read overwritten config")
                .contains("second-key")
        );
    }

    #[test]
    fn jsonl_requires_prompt() {
        let args = Args::try_parse_from(["morrow", "--jsonl"]).expect("parse jsonl");

        assert!(matches!(
            validate_jsonl_prompt(&args, ""),
            Err(CliError::JsonlRequiresPrompt)
        ));
        assert!(validate_jsonl_prompt(&args, "hello").is_ok());
    }

    #[test]
    fn effective_permissions_apply_cli_overrides() {
        let base = PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Deny,
        };

        assert_eq!(effective_permissions(base, None, false), base);
        assert_eq!(
            effective_permissions(base, Some(PermissionMode::DangerFullAccess), false),
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess)
        );
        assert_eq!(
            effective_permissions(base, None, true),
            PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Allow,
            }
        );
    }

    #[test]
    fn formats_file_change_approval_request_with_diff() {
        let request = ApprovalRequest::file_changes(
            "approval-call_1",
            vec![file_summary()],
            "--- /dev/null\n+++ note.txt\n@@\n+created\n",
            "file changes require approval",
        );

        let text = format_approval_request(
            &request,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        );

        assert!(text.contains("approval required: file changes require approval"));
        assert!(text.contains("action: file changes"));
        assert!(text.contains("- note.txt (add"));
        assert!(text.contains("+++ note.txt"));
        assert!(text.contains("permissions: mode=workspace_write, shell=prompt"));
    }

    #[test]
    fn formats_mcp_tool_approval_request() {
        let request = ApprovalRequest::mcp_tool(
            "approval-call_1",
            "docs",
            "write_page",
            r#"{"path":"/index","content":"hello"}"#,
            "MCP tool 'write_page' on server 'docs' requires approval",
        );

        let text = format_approval_request(
            &request,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        );

        assert!(text.contains("action: mcp tool"));
        assert!(text.contains("server: docs"));
        assert!(text.contains("tool: write_page"));
        assert!(text.contains(r#"arguments: {"path":"/index","content":"hello"}"#));
        assert!(text.contains("permissions: mode=workspace_write, shell=prompt"));
    }

    #[test]
    fn formats_execution_summary_for_file_shell_subagent_and_error_results() {
        let records = vec![
            ExecutionRecord {
                name: "write_file".to_string(),
                ok: true,
                summary: Some(ToolExecutionSummary::file_changes(
                    vec![file_summary()],
                    "--- /dev/null\n+++ note.txt\n@@\n+created\n",
                )),
            },
            ExecutionRecord {
                name: "shell_command".to_string(),
                ok: true,
                summary: Some(ToolExecutionSummary::shell(ShellCommandSummary {
                    command: "cargo test".to_string(),
                    exit_code: Some(0),
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })),
            },
            ExecutionRecord {
                name: "delegate_task".to_string(),
                ok: true,
                summary: Some(ToolExecutionSummary::subagent(
                    agent_protocol::SubagentExecutionSummary::success(
                        "Inspect runtime",
                        "Runtime is ready.",
                        2,
                        1,
                        false,
                    )
                    .with_agent_name("后藤一里"),
                )),
            },
            ExecutionRecord {
                name: "edit_file".to_string(),
                ok: false,
                summary: Some(ToolExecutionSummary::error("approval denied")),
            },
        ];

        let text = format_execution_summary(&records).expect("summary");

        assert!(text.contains("execution summary:"));
        assert!(text.contains("- write_file: ok"));
        assert!(text.contains("diff: available"));
        assert!(text.contains("shell: exit_code=0"));
        assert!(text.contains("agent: 后藤一里"));
        assert!(text.contains("task: Inspect runtime"));
        assert!(text.contains("- edit_file: error"));
        assert!(text.contains("error: approval denied"));
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

    #[tokio::test]
    async fn run_agent_turn_records_completed_turn_in_history_and_active_context() {
        let root = unique_cli_dir("run-success");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut output = Vec::new();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                tools: &ToolsConfig::default(),
                auto_approve_workspace_writes: true,
                interactive_approvals: false,
                output: OutputMode::Human,
            },
            &mut session,
            "hello",
            &mut output,
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
    }

    #[tokio::test]
    async fn run_agent_turn_jsonl_outputs_event_envelopes() {
        let root = unique_cli_dir("jsonl-text");
        let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut output = Vec::new();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                tools: &ToolsConfig::default(),
                auto_approve_workspace_writes: true,
                interactive_approvals: false,
                output: OutputMode::Jsonl {
                    session_name: "default",
                    turn_index: 0,
                },
            },
            &mut session,
            "hello",
            &mut output,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        let text = String::from_utf8(output).expect("utf8 output");
        let lines = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0]["schema_version"], json!(8));
        assert!(lines[0]["timestamp_ms"].as_u64().is_some());
        assert_eq!(lines[0]["session"], "default");
        assert_eq!(lines[0]["workspace_root"], root.display().to_string());
        assert_eq!(lines[0]["turn_index"], json!(0));
        assert_eq!(lines[0]["event_index"], json!(0));
        assert_eq!(lines[0]["event"], json!({"type": "turn_started"}));
        assert_eq!(lines[1]["event"], json!({"type": "model_call_started"}));
        assert_eq!(
            lines[2]["event"],
            json!({"type": "text_delta", "data": "ok"})
        );
        assert_eq!(
            lines[3]["event"],
            json!({
                "type": "model_message_committed",
                "data": {
                    "model_call_id": "model-call-0",
                    "message": {"role": "assistant", "content": "ok"}
                }
            })
        );
        assert_eq!(
            lines[4]["event"],
            json!({"type": "agent_message", "data": "ok"})
        );
        assert_eq!(lines[5]["event"], json!({"type": "turn_completed"}));
    }

    #[test]
    fn jsonl_subagent_events_include_the_assigned_name() {
        let root = unique_cli_dir("jsonl-subagent-name");
        let envelope = agent_runtime::make_event_envelope(
            "default",
            &root,
            2,
            3,
            AgentEvent::SubagentStarted {
                id: "call-1".to_string(),
                agent_id: Some("builtin-01".to_string()),
                agent_name: Some("后藤一里".to_string()),
                task: "Inspect runtime".to_string(),
            },
        );
        let mut output = Vec::new();

        write_jsonl_event(&mut output, &envelope).expect("write JSONL event");

        let value: serde_json::Value = serde_json::from_slice(&output).expect("parse JSONL event");
        assert_eq!(value["schema_version"], json!(8));
        assert_eq!(
            value["event"],
            json!({
                "type": "subagent_started",
                "data": {
                    "id": "call-1",
                    "agent_id": "builtin-01",
                    "agent_name": "后藤一里",
                    "task": "Inspect runtime"
                }
            })
        );
    }

    #[tokio::test]
    async fn run_agent_turn_jsonl_outputs_mcp_warning_events() {
        let root = unique_cli_dir("jsonl-mcp-warning");
        let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut output = Vec::new();
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
            require_approval: None,
        }];

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &mcp_servers,
                mcp_cache: &mcp_cache,
                tools: &ToolsConfig::default(),
                auto_approve_workspace_writes: true,
                interactive_approvals: false,
                output: OutputMode::Jsonl {
                    session_name: "default",
                    turn_index: 0,
                },
            },
            &mut session,
            "hello",
            &mut output,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        let text = String::from_utf8(output).expect("utf8 output");
        let lines = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert_eq!(lines[0]["event"]["type"], "warning");
        assert!(
            lines[0]["event"]["data"]
                .as_str()
                .expect("warning text")
                .contains("mcp server bad")
        );
        assert_eq!(lines[1]["event"], json!({"type": "turn_started"}));
        assert_eq!(
            lines.last().expect("last")["event"]["type"],
            "turn_completed"
        );
    }

    #[tokio::test]
    async fn run_agent_turn_jsonl_suppresses_human_execution_summary() {
        let root = unique_cli_dir("jsonl-tool");
        fs::write(root.join("note.txt"), "tool result\n").expect("write note");
        let first_body = tool_call_body(
            "call_1",
            "read_file",
            json!({"path": "note.txt", "max_lines": 5}),
        );
        let second_body = sse_text_body("done");
        let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut output = Vec::new();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                tools: &ToolsConfig::default(),
                auto_approve_workspace_writes: true,
                interactive_approvals: false,
                output: OutputMode::Jsonl {
                    session_name: "default",
                    turn_index: 0,
                },
            },
            &mut session,
            "read note",
            &mut output,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(!text.contains("execution summary:"));
        let lines = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .any(|line| line["event"]["type"] == "tool_call_finished")
        );
    }

    #[tokio::test]
    async fn auto_compaction_failure_records_failed_turn_without_main_model_call() {
        let root = unique_cli_dir("run-compact-fail");
        let (base_url, requests) = spawn_recording_sse_server(vec!["data: {not-json}\n\n"]).await;
        let client = client(base_url);
        let mut session = compactable_session();
        session.active_thread.push(Message::user(
            "large active context that exceeds the tiny budget",
        ));
        let mut output = Vec::new();
        let mcp_cache = McpToolCache::new();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(1),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &mcp_cache,
                tools: &ToolsConfig::default(),
                auto_approve_workspace_writes: true,
                interactive_approvals: false,
                output: OutputMode::Human,
            },
            &mut session,
            "hello",
            &mut output,
        )
        .await
        .expect("run turn");

        assert!(matches!(
            outcome,
            RunAgentTurnOutcome {
                session_changed: true,
                error: Some(_),
            }
        ));
        assert!(session.context.summary.is_some());
        assert_ne!(session.context.summarized_turns, 0);
        assert_eq!(session.turns.len(), 6);
        assert_eq!(
            session.turns.last().expect("failed turn").turn.status,
            TurnStatus::Failed
        );
        assert_eq!(
            session
                .turns
                .last()
                .expect("failed turn")
                .turn
                .model
                .as_ref(),
            Some(test_model_invocation())
        );
        assert!(
            session
                .turns
                .last()
                .expect("failed turn")
                .turn
                .error
                .as_deref()
                .expect("error")
                .contains("context compaction failed")
        );
        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
    }
}
