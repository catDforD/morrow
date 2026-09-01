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
    SessionStore, SubagentSessionStore, TurnEventHandler, WorkspaceInstructionsCache,
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

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

mod approval_ui;
pub use approval_ui::*;
mod cli;
pub use cli::*;
mod commands_hooks;
pub use commands_hooks::*;
mod commands_init;
pub use commands_init::*;
mod commands_session;
pub use commands_session::*;
mod output;
pub use output::*;
mod repl;
pub use repl::*;
mod run;
pub use run::*;
#[cfg(test)]
mod tests;
