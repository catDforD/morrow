use agent_config::load_config;
use agent_core::{Agent, AgentError};
use agent_model::{ModelError, OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{
    AgentEvent, ApprovalAction, ApprovalDecision, ApprovalRequest, PermissionMode,
    PermissionProfile, ShellPolicy, Thread,
};
use agent_tools::{ToolRegistry, ToolRegistryError};
use clap::Parser;
use futures_util::StreamExt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use thread_store::ThreadStore;

mod thread_store;

#[derive(Debug, Parser)]
#[command(name = "morrow")]
#[command(about = "Minimal OpenAI-compatible agent loop CLI")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "default")]
    thread: String,

    #[arg(long)]
    reset_thread: bool,

    #[arg(long, value_parser = parse_permission_mode)]
    permission: Option<PermissionMode>,

    #[arg(long)]
    allow_shell: bool,

    #[arg(value_name = "PROMPT", num_args = 0..)]
    prompt: Vec<String>,
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Config(#[from] agent_config::ConfigError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    ThreadStore(#[from] thread_store::ThreadStoreError),
    #[error(transparent)]
    Tools(#[from] ToolRegistryError),
    #[error("agent run failed: {0}")]
    AgentRun(String),
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
    let loaded = load_config(args.config.as_deref())?;
    let permissions =
        effective_permissions(loaded.config.permissions, args.permission, args.allow_shell);
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: loaded.config.model.base_url,
        model: loaded.config.model.model,
        api_key: loaded.api_key,
        timeout: Duration::from_secs(loaded.config.model.timeout_secs),
    })?;
    let thread_store = ThreadStore::for_current_dir(&args.thread)?;
    let mut thread = if args.reset_thread {
        Thread::new()
    } else {
        thread_store.load()?
    };
    let workspace_root = detect_workspace_root()?;
    let prompt = args.prompt.join(" ");

    if prompt.trim().is_empty() {
        let mut permissions = permissions;
        run_repl(
            ReplContext {
                client: &client,
                system_prompt: &loaded.config.agent.system_prompt,
                thread_store: &thread_store,
                thread_name: &args.thread,
                workspace_root: &workspace_root,
                config_path: &loaded.path,
            },
            &mut thread,
            &mut permissions,
        )
        .await?;
        return Ok(());
    }

    let turn_completed = run_agent_turn(
        &client,
        &loaded.config.agent.system_prompt,
        &mut thread,
        &workspace_root,
        permissions,
        &prompt,
        io::stdin().is_terminal(),
    )
    .await?;

    if turn_completed {
        thread_store.save(&thread)?;
    }

    Ok(())
}

struct ReplContext<'a> {
    client: &'a OpenAiCompatClient,
    system_prompt: &'a str,
    thread_store: &'a ThreadStore,
    thread_name: &'a str,
    workspace_root: &'a Path,
    config_path: &'a Path,
}

async fn run_repl(
    context: ReplContext<'_>,
    thread: &mut Thread,
    permissions: &mut PermissionProfile,
) -> Result<(), CliError> {
    eprintln!("morrow interactive mode. Type /exit to quit.");

    loop {
        eprint!("morrow> ");
        io::stderr().flush().map_err(CliError::Stderr)?;

        let mut input = String::new();
        let read = io::stdin().read_line(&mut input).map_err(CliError::Stdin)?;
        if read == 0 {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if input.starts_with('/') {
            if handle_repl_command(
                input,
                context.thread_store,
                context.thread_name,
                thread,
                context.workspace_root,
                context.config_path,
                permissions,
            )? {
                break;
            }
            continue;
        }

        let turn_completed = run_agent_turn(
            context.client,
            context.system_prompt,
            thread,
            context.workspace_root,
            *permissions,
            input,
            io::stdin().is_terminal(),
        )
        .await?;

        if turn_completed {
            context.thread_store.save(thread)?;
        }
    }

    Ok(())
}

fn handle_repl_command(
    input: &str,
    thread_store: &ThreadStore,
    thread_name: &str,
    thread: &mut Thread,
    workspace_root: &Path,
    config_path: &Path,
    permissions: &mut PermissionProfile,
) -> Result<bool, CliError> {
    let mut parts = input.split_whitespace();
    let command = parts.next().unwrap_or_default();

    match command {
        "/exit" | "/quit" => Ok(true),
        "/status" => {
            eprintln!("thread: {thread_name}");
            eprintln!("workspace: {}", workspace_root.display());
            eprintln!("config: {}", config_path.display());
            eprintln!("permissions: {}", permission_summary(*permissions));
            Ok(false)
        }
        "/reset" => {
            *thread = Thread::new();
            thread_store.save(thread)?;
            eprintln!("thread reset");
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

async fn run_agent_turn(
    client: &OpenAiCompatClient,
    system_prompt: &str,
    thread: &mut Thread,
    workspace_root: &Path,
    permissions: PermissionProfile,
    prompt: &str,
    interactive_approvals: bool,
) -> Result<bool, CliError> {
    let tools = ToolRegistry::built_in(workspace_root, permissions)?;
    let agent = Agent::with_tools(client.clone(), system_prompt.to_string(), tools);
    let mut stdout = io::stdout().lock();
    let mut wrote_text = false;
    let mut output_ends_with_newline = false;
    let mut agent_error = None;
    let mut turn_completed = false;

    {
        let mut stream = agent.run_turn(thread, prompt.to_string()).await?;

        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TurnStarted => {}
                AgentEvent::TextDelta(text) => {
                    wrote_text = true;
                    output_ends_with_newline = text.ends_with('\n');
                    stdout
                        .write_all(text.as_bytes())
                        .map_err(CliError::Stdout)?;
                    stdout.flush().map_err(CliError::Stdout)?;
                }
                AgentEvent::AgentMessage(_) => {}
                AgentEvent::ToolCallStarted { name, .. } => {
                    eprintln!("tool {name} started");
                }
                AgentEvent::ToolCallFinished { name, ok, .. } => {
                    let status = if ok { "ok" } else { "error" };
                    eprintln!("tool {name} {status}");
                }
                AgentEvent::ApprovalRequested(request) => {
                    let decision = approval_decision(&request, permissions, interactive_approvals)?;
                    stream.resolve_approval(decision)?;
                }
                AgentEvent::ApprovalResolved(decision) => {
                    let status = if decision.approved {
                        "approved"
                    } else {
                        "denied"
                    };
                    eprintln!("approval {} {status}", decision.request_id);
                }
                AgentEvent::TurnCompleted => {
                    if wrote_text && !output_ends_with_newline {
                        stdout.write_all(b"\n").map_err(CliError::Stdout)?;
                        stdout.flush().map_err(CliError::Stdout)?;
                    }
                    turn_completed = true;
                }
                AgentEvent::Error(message) => {
                    agent_error = Some(message);
                }
            }
        }
    }

    if let Some(message) = agent_error {
        return Err(CliError::AgentRun(message));
    }

    Ok(turn_completed)
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

    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(CliError::Stdin)?;
    let approved = matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    Ok(if approved {
        ApprovalDecision::approve(request.id.clone())
    } else {
        ApprovalDecision::deny(request.id.clone())
    })
}

fn print_approval_request(request: &ApprovalRequest, permissions: PermissionProfile) {
    eprintln!("approval required: {}", request.reason);
    match &request.action {
        ApprovalAction::ShellCommand {
            command,
            cwd,
            timeout_secs,
        } => {
            eprintln!("action: shell command");
            eprintln!("command: {command}");
            eprintln!("cwd: {}", cwd.display());
            eprintln!("timeout: {timeout_secs}s");
            eprintln!("permissions: {}", permission_summary(permissions));
            eprintln!("warning: approving this command may modify files or access the network.");
        }
    }
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

fn detect_workspace_root() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir().map_err(thread_store::ThreadStoreError::CurrentDir)?;
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

fn manifest_has_workspace_header(path: &std::path::Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| line.trim() == "[workspace]")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
