use super::*;

pub(crate) struct ReplContext<'a> {
    pub(crate) client: &'a OpenAiCompatClient,
    pub(crate) model: &'a ModelInvocation,
    pub(crate) system_prompt: &'a str,
    pub(crate) workspace_instructions: &'a WorkspaceInstructionsCache,
    pub(crate) context_config: ContextConfig,
    pub(crate) model_limits: ModelContextLimits,
    pub(crate) session_handle: &'a SessionHandle,
    pub(crate) session_name: &'a str,
    pub(crate) workspace_root: &'a Path,
    pub(crate) session_scope_root: &'a Path,
    pub(crate) config_path: &'a Path,
    pub(crate) mcp_servers: &'a [McpServerConfig],
    pub(crate) mcp_cache: &'a McpToolCache,
    pub(crate) tools: &'a ToolsConfig,
    pub(crate) auto_approve_workspace_writes: bool,
    pub(crate) subagent_store_path: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputLine {
    pub(crate) text: String,
    pub(crate) had_invalid_utf8: bool,
}

pub(crate) async fn run_repl(
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
                workspace_instructions: Some(context.workspace_instructions),
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

pub(crate) fn read_stdin_line() -> Result<Option<InputLine>, CliError> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    read_input_line(&mut stdin).map_err(CliError::Stdin)
}

pub(crate) fn read_input_line(reader: &mut impl BufRead) -> io::Result<Option<InputLine>> {
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
