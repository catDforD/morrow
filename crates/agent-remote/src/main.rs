use agent_remote::{run_host, run_workspace_agent};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "morrow-remote")]
#[command(about = "Morrow remote workspace runtime")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Host {
        #[arg(long)]
        stdio: bool,
    },
    WorkspaceAgent {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        stdio: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("morrow-remote: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Host { stdio } => {
            require_stdio(stdio)?;
            run_host(tokio::io::stdin(), tokio::io::stdout()).await?;
        }
        Command::WorkspaceAgent { workspace, stdio } => {
            require_stdio(stdio)?;
            run_workspace_agent(workspace, tokio::io::stdin(), tokio::io::stdout()).await?;
        }
    }
    Ok(())
}

fn require_stdio(stdio: bool) -> Result<(), &'static str> {
    if stdio {
        Ok(())
    } else {
        Err("--stdio is required")
    }
}
