use agent_config::load_config;
use agent_core::{Agent, AgentError};
use agent_model::{ModelError, OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{AgentEvent, Thread};
use clap::Parser;
use futures_util::StreamExt;
use std::io::{self, Write};
use std::path::PathBuf;
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

    prompt: String,
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
    #[error("agent run failed: {0}")]
    AgentRun(String),
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
    let agent = Agent::new(client, loaded.config.agent.system_prompt);
    let mut stdout = io::stdout().lock();
    let mut wrote_text = false;
    let mut output_ends_with_newline = false;
    let mut agent_error = None;
    let mut turn_completed = false;

    {
        let mut stream = agent.run_turn(&mut thread, args.prompt).await?;

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

    if turn_completed {
        thread_store.save(&thread)?;
    }

    Ok(())
}
