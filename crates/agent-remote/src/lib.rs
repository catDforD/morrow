pub mod client;
pub mod framing;

use agent_config::load_server_config_for_workspace;
use agent_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteDirectoryEntry, RemoteDirectoryListing, RemoteEnvelope,
    RemoteEnvironment, RemoteError, RemoteEvent, RemoteFallbackModelSpec, RemoteHello,
    RemoteMcpServerSummary, RemoteMcpTransport, RemoteMessage, RemoteRequest, RemoteResponse,
    RemoteRole, RemoteWorkspaceConfiguration, RemoteWorkspaceInfo,
};
use agent_server::{EmbeddedServer, discover_remote_models, server_options_from_loaded_config};
use client::RemoteClient;
use framing::{FramedReader, FramedWriter, FramingError};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, watch};

#[derive(Debug, Error)]
pub enum RemoteRuntimeError {
    #[error(transparent)]
    Framing(#[from] FramingError),
    #[error(transparent)]
    Client(#[from] client::RemoteClientError),
    #[error("remote protocol version {0} is unsupported")]
    Protocol(u32),
    #[error("remote handshake was invalid")]
    Handshake,
    #[error("remote runtime version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: String, actual: String },
    #[error("home directory was not found")]
    HomeNotFound,
    #[error("failed to initialize workspace: {0}")]
    Workspace(String),
    #[error("failed to launch workspace agent: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("workspace agent has no {0} pipe")]
    MissingPipe(&'static str),
}

struct WorkspaceProcess {
    child_task: tokio::task::JoinHandle<()>,
    exit: watch::Receiver<Option<WorkspaceExit>>,
    process_id: Option<u32>,
    client: RemoteClient,
    channel_id: u32,
    path: PathBuf,
}

#[derive(Clone, Copy)]
struct WorkspaceExit {
    code: Option<i32>,
}

pub async fn run_host<R, W>(reader: R, writer: W) -> Result<(), RemoteRuntimeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    reject_root().await?;
    let mut reader = FramedReader::new(reader);
    let mut writer = FramedWriter::new(writer);
    server_handshake(&mut reader, &mut writer, RemoteRole::Host).await?;

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<RemoteEnvelope>(128);
    let writer_task = tokio::spawn(async move {
        while let Some(envelope) = outbound_rx.recv().await {
            if writer.write_frame(&envelope).await.is_err() {
                break;
            }
        }
    });
    let workspace = Arc::new(Mutex::new(None::<WorkspaceProcess>));
    let mut requests = tokio::task::JoinSet::new();
    let read_result = async {
        while let Some(envelope) = reader.read_frame().await? {
            if envelope.protocol_version != REMOTE_PROTOCOL_VERSION {
                return Err(RemoteRuntimeError::Protocol(envelope.protocol_version));
            }
            let RemoteMessage::Request(request) = envelope.message else {
                continue;
            };
            if matches!(request, RemoteRequest::Shutdown { .. }) {
                requests.abort_all();
                while requests.join_next().await.is_some() {}
                let response = handle_host_request(
                    request,
                    envelope.channel_id,
                    workspace.clone(),
                    outbound_tx.clone(),
                )
                .await;
                send_response(
                    &outbound_tx,
                    envelope.channel_id,
                    envelope.request_id,
                    response,
                )
                .await?;
                break;
            }
            let workspace = workspace.clone();
            let outbound = outbound_tx.clone();
            requests.spawn(async move {
                let response =
                    handle_host_request(request, envelope.channel_id, workspace, outbound.clone())
                        .await;
                let _ = send_response(
                    &outbound,
                    envelope.channel_id,
                    envelope.request_id,
                    response,
                )
                .await;
            });
            while requests.try_join_next().is_some() {}
        }
        Ok::<(), RemoteRuntimeError>(())
    }
    .await;
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    close_workspace(&workspace, true).await;
    drop(outbound_tx);
    let _ = writer_task.await;
    read_result
}

pub async fn run_workspace_agent<R, W>(
    workspace: PathBuf,
    reader: R,
    writer: W,
) -> Result<(), RemoteRuntimeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let workspace = workspace
        .canonicalize()
        .map_err(|error| RemoteRuntimeError::Workspace(error.to_string()))?;
    if !workspace.is_dir() {
        return Err(RemoteRuntimeError::Workspace(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    let home = dirs::home_dir().ok_or(RemoteRuntimeError::HomeNotFound)?;
    let loaded = load_server_config_for_workspace(None, &workspace)
        .map_err(|error| RemoteRuntimeError::Workspace(error.to_string()))?;
    let workspace_configuration = remote_workspace_configuration(&loaded);
    let mut options = server_options_from_loaded_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        workspace,
        &home,
        loaded,
        "default".to_string(),
    )
    .map_err(|error| RemoteRuntimeError::Workspace(error.to_string()))?;
    options.workspace_location = agent_protocol::WorkspaceLocation::Wsl {
        distro: std::env::var("WSL_DISTRO_NAME").unwrap_or_else(|_| "WSL".to_string()),
        user: std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".to_string()),
        path: options.workspace_root.display().to_string(),
    };
    let server = Arc::new(
        EmbeddedServer::new_workspace(options)
            .map_err(|error| RemoteRuntimeError::Workspace(error.to_string()))?,
    );

    let mut reader = FramedReader::new(reader);
    let mut writer = FramedWriter::new(writer);
    server_handshake(&mut reader, &mut writer, RemoteRole::WorkspaceAgent).await?;
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<RemoteEnvelope>(128);
    let writer_task = tokio::spawn(async move {
        while let Some(envelope) = outbound_rx.recv().await {
            if writer.write_frame(&envelope).await.is_err() {
                break;
            }
        }
    });
    let subscriptions = Arc::new(Mutex::new(
        HashMap::<String, tokio::task::AbortHandle>::new(),
    ));
    let workspace_configuration = Arc::new(workspace_configuration);
    let mut requests = tokio::task::JoinSet::new();
    let read_result = async {
        while let Some(envelope) = reader.read_frame().await? {
            if envelope.protocol_version != REMOTE_PROTOCOL_VERSION {
                return Err(RemoteRuntimeError::Protocol(envelope.protocol_version));
            }
            let RemoteMessage::Request(request) = envelope.message else {
                continue;
            };
            if matches!(request, RemoteRequest::Shutdown { .. }) {
                requests.abort_all();
                while requests.join_next().await.is_some() {}
                let response = handle_workspace_request(
                    request,
                    envelope.channel_id,
                    server.clone(),
                    workspace_configuration.as_ref(),
                    subscriptions.clone(),
                    outbound_tx.clone(),
                )
                .await;
                send_response(
                    &outbound_tx,
                    envelope.channel_id,
                    envelope.request_id,
                    response,
                )
                .await?;
                break;
            }
            let server = server.clone();
            let configuration = workspace_configuration.clone();
            let subscriptions = subscriptions.clone();
            let outbound = outbound_tx.clone();
            requests.spawn(async move {
                let response = handle_workspace_request(
                    request,
                    envelope.channel_id,
                    server,
                    configuration.as_ref(),
                    subscriptions,
                    outbound.clone(),
                )
                .await;
                let _ = send_response(
                    &outbound,
                    envelope.channel_id,
                    envelope.request_id,
                    response,
                )
                .await;
            });
            while requests.try_join_next().is_some() {}
        }
        Ok::<(), RemoteRuntimeError>(())
    }
    .await;
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    for (_, handle) in subscriptions.lock().await.drain() {
        handle.abort();
    }
    server.shutdown(true).await;
    drop(outbound_tx);
    let _ = writer_task.await;
    read_result
}

async fn send_response(
    outbound: &mpsc::Sender<RemoteEnvelope>,
    channel_id: u32,
    request_id: String,
    response: RemoteResponse,
) -> Result<(), RemoteRuntimeError> {
    outbound
        .send(RemoteEnvelope::new(
            channel_id,
            request_id,
            RemoteMessage::Response(response),
        ))
        .await
        .map_err(|_| FramingError::Eof)?;
    Ok(())
}

async fn server_handshake<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
    role: RemoteRole,
) -> Result<(), RemoteRuntimeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer
        .write_handshake(&RemoteEnvelope::new(
            0,
            "hello",
            RemoteMessage::Hello(RemoteHello {
                version: env!("CARGO_PKG_VERSION").to_string(),
                platform: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                pid: std::process::id(),
                role,
            }),
        ))
        .await?;
    let ack = reader.read_handshake::<RemoteEnvelope>().await?;
    if ack.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err(RemoteRuntimeError::Protocol(ack.protocol_version));
    }
    if ack.channel_id != 0 {
        return Err(RemoteRuntimeError::Handshake);
    }
    let RemoteMessage::HelloAck(ack) = ack.message else {
        return Err(RemoteRuntimeError::Handshake);
    };
    if ack.desktop_version != env!("CARGO_PKG_VERSION") {
        return Err(RemoteRuntimeError::VersionMismatch {
            expected: env!("CARGO_PKG_VERSION").to_string(),
            actual: ack.desktop_version,
        });
    }
    Ok(())
}

async fn handle_host_request(
    request: RemoteRequest,
    channel_id: u32,
    workspace: Arc<Mutex<Option<WorkspaceProcess>>>,
    outbound: mpsc::Sender<RemoteEnvelope>,
) -> RemoteResponse {
    match request {
        RemoteRequest::Ping => RemoteResponse::Pong,
        RemoteRequest::Activity => {
            let client = workspace
                .lock()
                .await
                .as_ref()
                .map(|process| process.client.clone());
            let Some(client) = client else {
                return RemoteResponse::Activity(agent_protocol::RemoteActivity {
                    running_turns: 0,
                    pending_approvals: 0,
                });
            };
            match client
                .request(channel_id.max(1), RemoteRequest::Activity)
                .await
            {
                Ok(response) => response,
                Err(error) => remote_error("workspace_request", error.to_string()),
            }
        }
        RemoteRequest::Environment => match remote_environment() {
            Ok(environment) => RemoteResponse::Environment(environment),
            Err(error) => remote_error("environment", error),
        },
        RemoteRequest::ListDirectory { path, show_hidden } => {
            match list_directory(path.as_deref(), show_hidden) {
                Ok(listing) => RemoteResponse::Directory(listing),
                Err(error) => remote_error("directory", error),
            }
        }
        RemoteRequest::OpenWorkspace { path } => {
            close_workspace(&workspace, true).await;
            match spawn_workspace(Path::new(&path), channel_id.max(1)).await {
                Ok(process) => {
                    let info = RemoteWorkspaceInfo {
                        channel_id: process.channel_id,
                        path: process.path.display().to_string(),
                        name: process
                            .path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("workspace")
                            .to_string(),
                    };
                    let mut events = process.client.subscribe_events();
                    let outbound_for_events = outbound.clone();
                    tokio::spawn(async move {
                        loop {
                            match events.recv().await {
                                Ok(event) => {
                                    if outbound_for_events.send(event).await.is_err() {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                    let mut exit = process.exit.clone();
                    let outbound_for_exit = outbound.clone();
                    let worker_channel = process.channel_id;
                    tokio::spawn(async move {
                        if exit.borrow().is_none() && exit.changed().await.is_err() {
                            return;
                        }
                        let code = exit.borrow().as_ref().and_then(|status| status.code);
                        let _ = outbound_for_exit
                            .send(RemoteEnvelope::new(
                                worker_channel,
                                format!("worker-exited-{worker_channel}"),
                                RemoteMessage::Event(RemoteEvent::WorkerExited {
                                    channel_id: worker_channel,
                                    code,
                                }),
                            ))
                            .await;
                    });
                    let replaced = workspace.lock().await.replace(process);
                    if let Some(replaced) = replaced {
                        close_workspace_process(replaced, true).await;
                    }
                    RemoteResponse::WorkspaceOpened(info)
                }
                Err(error) => remote_error("workspace_open", error.to_string()),
            }
        }
        RemoteRequest::CloseWorkspace => {
            close_workspace(&workspace, false).await;
            RemoteResponse::Ack
        }
        RemoteRequest::Shutdown { cancel_running } => {
            close_workspace(&workspace, cancel_running).await;
            RemoteResponse::Ack
        }
        forwarded => {
            let client = workspace
                .lock()
                .await
                .as_ref()
                .map(|process| process.client.clone());
            let Some(client) = client else {
                return remote_error("workspace_unavailable", "no workspace is open");
            };
            match client.request(channel_id.max(1), forwarded).await {
                Ok(response) => response,
                Err(error) => remote_error("workspace_request", error.to_string()),
            }
        }
    }
}

async fn handle_workspace_request(
    request: RemoteRequest,
    channel_id: u32,
    server: Arc<EmbeddedServer>,
    workspace_configuration: &RemoteWorkspaceConfiguration,
    subscriptions: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    outbound: mpsc::Sender<RemoteEnvelope>,
) -> RemoteResponse {
    match request {
        RemoteRequest::Ping => RemoteResponse::Pong,
        RemoteRequest::Activity => {
            let activity = server.activity().await;
            RemoteResponse::Activity(agent_protocol::RemoteActivity {
                running_turns: activity.running_turns,
                pending_approvals: activity.pending_approvals,
            })
        }
        RemoteRequest::WorkspaceConfiguration => {
            RemoteResponse::WorkspaceConfiguration(workspace_configuration.clone())
        }
        RemoteRequest::Http { method, path, body } => {
            match server.request(&method, &path, body).await {
                Ok(response) => RemoteResponse::Http(agent_protocol::RemoteHttpResponse {
                    status: response.status,
                    body: response.body,
                }),
                Err(error) => remote_error("http", error),
            }
        }
        RemoteRequest::SubscribeSession { session } => {
            match server.subscribe_session(&session).await {
                Ok(mut subscription) => {
                    let subscription_id = format!("subscription-{channel_id}-{session}");
                    let snapshot = subscription.snapshot.clone();
                    if let Some(handle) = subscriptions.lock().await.remove(&subscription_id) {
                        handle.abort();
                    }
                    let id_for_task = subscription_id.clone();
                    let outbound_for_task = outbound.clone();
                    let task = tokio::spawn(async move {
                        while let Ok(message) = subscription.recv().await {
                            let envelope = RemoteEnvelope::new(
                                channel_id,
                                format!("event-{id_for_task}"),
                                RemoteMessage::Event(RemoteEvent::SessionMessage {
                                    subscription_id: id_for_task.clone(),
                                    message,
                                }),
                            );
                            if outbound_for_task.send(envelope).await.is_err() {
                                break;
                            }
                        }
                    });
                    subscriptions
                        .lock()
                        .await
                        .insert(subscription_id.clone(), task.abort_handle());
                    RemoteResponse::SessionSubscribed {
                        subscription_id,
                        snapshot,
                    }
                }
                Err(error) => remote_error("session_subscribe", error),
            }
        }
        RemoteRequest::UnsubscribeSession { subscription_id } => {
            if let Some(handle) = subscriptions.lock().await.remove(&subscription_id) {
                handle.abort();
            }
            RemoteResponse::Ack
        }
        RemoteRequest::SessionMessage { session, message } => {
            match server.send_session_message(&session, message).await {
                Ok(()) => RemoteResponse::Ack,
                Err(error) => remote_error("session_message", error),
            }
        }
        RemoteRequest::StartTurn { turn } => match server.start_remote_turn(*turn).await {
            Ok(()) => RemoteResponse::Ack,
            Err(error) => remote_error("start_turn", error),
        },
        RemoteRequest::InspectMcp { server: mcp_server } => {
            let inspection = server.inspect_remote_mcp(*mcp_server).await;
            match serde_json::to_value(inspection) {
                Ok(body) => RemoteResponse::Http(agent_protocol::RemoteHttpResponse {
                    status: 200,
                    body: Some(body),
                }),
                Err(error) => remote_error("mcp_inspection", error.to_string()),
            }
        }
        RemoteRequest::DiscoverModels { model } => match discover_remote_models(model).await {
            Ok(models) => match serde_json::to_value(models) {
                Ok(body) => RemoteResponse::Http(agent_protocol::RemoteHttpResponse {
                    status: 200,
                    body: Some(body),
                }),
                Err(error) => remote_error("model_discovery", error.to_string()),
            },
            Err(error) => remote_error("model_discovery", error.to_string()),
        },
        RemoteRequest::Shutdown { cancel_running } => {
            server.shutdown(cancel_running).await;
            RemoteResponse::Ack
        }
        _ => remote_error("unsupported", "request is not supported by workspace agent"),
    }
}

fn remote_workspace_configuration(
    loaded: &agent_config::LoadedServerConfig,
) -> RemoteWorkspaceConfiguration {
    let fallback_model = loaded.model.as_ref().map(|model| {
        let model_name = model.config.model.clone();
        RemoteFallbackModelSpec {
            provider_name: "默认配置".to_string(),
            model_id: model_name.clone(),
            model_name: model_name.clone(),
            context_window_tokens: model.config.context_window_tokens,
            reserved_output_tokens: model.config.reserved_output_tokens,
            reasoning_profile: match model_name.as_str() {
                "deepseek-v4-flash" | "deepseek-v4-pro" => {
                    agent_protocol::ReasoningProfile::Deepseek
                }
                _ => agent_protocol::ReasoningProfile::None,
            },
        }
    });
    let fallback_mcp_servers = loaded
        .config
        .mcp_servers
        .iter()
        .map(|server| RemoteMcpServerSummary {
            name: server.name.clone(),
            transport: match server.transport {
                agent_config::McpTransport::Stdio => RemoteMcpTransport::Stdio,
                agent_config::McpTransport::Http => RemoteMcpTransport::Http,
            },
            command: server.command.clone(),
            args: server.args.clone(),
            env_keys: server.env.keys().cloned().collect(),
            cwd: server.cwd.as_ref().map(|path| path.display().to_string()),
            url: server.url.clone(),
            http_header_keys: server.http_headers.keys().cloned().collect(),
            enabled: server.enabled,
            startup_timeout_sec: server.startup_timeout_sec,
            tool_timeout_sec: server.tool_timeout_sec,
        })
        .collect();
    RemoteWorkspaceConfiguration {
        fallback_model,
        fallback_mcp_servers,
    }
}

async fn spawn_workspace(
    path: &Path,
    channel_id: u32,
) -> Result<WorkspaceProcess, RemoteRuntimeError> {
    let path = path
        .canonicalize()
        .map_err(|error| RemoteRuntimeError::Workspace(error.to_string()))?;
    if !path.is_dir() {
        return Err(RemoteRuntimeError::Workspace(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    let executable = std::env::current_exe().map_err(RemoteRuntimeError::Spawn)?;
    let mut command = Command::new(executable);
    command
        .arg("workspace-agent")
        .arg("--workspace")
        .arg(&path)
        .arg("--stdio")
        .current_dir(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(RemoteRuntimeError::Spawn)?;
    let stdin = child
        .stdin
        .take()
        .ok_or(RemoteRuntimeError::MissingPipe("stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(RemoteRuntimeError::MissingPipe("stdout"))?;
    let client = RemoteClient::connect(stdout, stdin).await?;
    if client.hello().role != RemoteRole::WorkspaceAgent {
        return Err(RemoteRuntimeError::Handshake);
    }
    let process_id = child.id();
    let (exit_tx, exit) = watch::channel(None);
    let child_task = tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|status| status.code());
        let _ = exit_tx.send(Some(WorkspaceExit { code }));
    });
    Ok(WorkspaceProcess {
        child_task,
        exit,
        process_id,
        client,
        channel_id,
        path,
    })
}

async fn close_workspace(workspace: &Arc<Mutex<Option<WorkspaceProcess>>>, cancel_running: bool) {
    let process = workspace.lock().await.take();
    let Some(process) = process else {
        return;
    };
    close_workspace_process(process, cancel_running).await;
}

async fn close_workspace_process(mut process: WorkspaceProcess, cancel_running: bool) {
    let _ = process
        .client
        .request(1, RemoteRequest::Shutdown { cancel_running })
        .await;
    let exited = if process.exit.borrow().is_some() {
        true
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(3), process.exit.changed())
            .await
            .is_ok()
    };
    if !exited {
        if let Some(process_id) = process.process_id {
            kill_process_group(process_id);
        }
        process.child_task.abort();
    }
    let _ = process.child_task.await;
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) {
    unsafe extern "C" {
        fn kill(process_id: i32, signal: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let Ok(process_id) = i32::try_from(process_id) else {
        return;
    };
    // SAFETY: kill only reads the integer arguments; a negative pid targets the process group.
    let _ = unsafe { kill(-process_id, SIGKILL) };
}

#[cfg(not(unix))]
fn kill_process_group(_process_id: u32) {}

fn remote_environment() -> Result<RemoteEnvironment, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory was not found".to_string())?;
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    Ok(RemoteEnvironment {
        user,
        home: home.display().to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    })
}

fn list_directory(path: Option<&str>, show_hidden: bool) -> Result<RemoteDirectoryListing, String> {
    let path = match path {
        Some(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => dirs::home_dir().ok_or_else(|| "home directory was not found".to_string())?,
    };
    let path = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let mut entries = std::fs::read_dir(&path)
        .map_err(|error| format!("failed to list {}: {error}", path.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let hidden = name.starts_with('.');
            if hidden && !show_hidden {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some(RemoteDirectoryEntry {
                name,
                path: entry.path().display().to_string(),
                directory: metadata.is_dir(),
                hidden,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(RemoteDirectoryListing {
        path: path.display().to_string(),
        parent: path.parent().map(|parent| parent.display().to_string()),
        entries,
    })
}

async fn reject_root() -> Result<(), RemoteRuntimeError> {
    #[cfg(unix)]
    {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .await
            .map_err(RemoteRuntimeError::Spawn)?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0" {
            return Err(RemoteRuntimeError::Workspace(
                "morrow remote host refuses to run as root".to_string(),
            ));
        }
    }
    Ok(())
}

fn remote_error(code: impl Into<String>, message: impl Into<String>) -> RemoteResponse {
    RemoteResponse::Error(RemoteError {
        code: code.into(),
        message: message.into(),
    })
}
