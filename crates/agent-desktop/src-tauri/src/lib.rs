mod atomic_file;
mod state;
mod wsl;

use agent_config::{
    ContextConfig, McpServerConfig, McpTransport, load_server_config_for_workspace,
};
use agent_protocol::{
    PermissionProfile, RemoteEnvelope, RemoteEvent, RemoteMcpTransport, RemoteMessage,
    RemoteRequest, RemoteResponse, RemoteWorkspaceConfiguration, WorkspaceLocation,
};
use agent_server::{
    EmbeddedServer, FallbackModel, RunningServer, ServerAccessPolicy, ServerOptions,
    ShutdownPolicy, server_options_from_loaded_config, spawn_local,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use state::{DesktopState, DesktopStateError};
use std::collections::HashMap;
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
use std::collections::HashSet;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, RwLock};
use std::time::Duration;
use tauri::ipc::Channel;
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, State, Url, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind};
use tauri_plugin_opener::OpenerExt;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};
use wsl::{WslConnection, WslDistribution, WslProbe};

const MAIN_WINDOW_LABEL: &str = "main";
#[cfg(target_os = "macos")]
const OPEN_FOLDER_MENU_ID: &str = "file.open-folder";
#[cfg(target_os = "macos")]
const OPEN_RECENT_PREFIX: &str = "file.open-recent.";
#[cfg(target_os = "macos")]
const DOWNLOAD_RELEASE_MENU_ID: &str = "help.download-release";
#[cfg(target_os = "macos")]
const OPEN_LOGS_MENU_ID: &str = "help.open-logs";
const RELEASES_URL: &str = "https://github.com/catDforD/morrow/releases/latest";
const VITE_DEV_URL: &str = "http://127.0.0.1:5173/";
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_SHELL_STATE_PERMISSION: &str = "allow-desktop-shell-state";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_ACTION_PERMISSION: &str = "allow-desktop-action";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_WSL_DISTRIBUTIONS_PERMISSION: &str = "allow-desktop-wsl-distributions";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_WSL_PROBE_PERMISSION: &str = "allow-desktop-wsl-probe";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_WSL_PREPARE_PERMISSION: &str = "allow-desktop-wsl-prepare";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_WSL_CONNECT_PERMISSION: &str = "allow-desktop-wsl-connect";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_REMOTE_REQUEST_PERMISSION: &str = "allow-desktop-remote-request";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_REMOTE_SUBSCRIBE_PERMISSION: &str = "allow-desktop-remote-subscribe";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
const DESKTOP_REMOTE_UNSUBSCRIBE_PERMISSION: &str = "allow-desktop-remote-unsubscribe";

struct DesktopRuntime {
    home: PathBuf,
    state_path: PathBuf,
    bootstrap_token: String,
    inner: Mutex<DesktopRuntimeInner>,
    operation: Mutex<()>,
    navigation_origin: RwLock<Option<String>>,
    app_url: RwLock<Option<Url>>,
    remote_channels: StdMutex<HashMap<u64, Channel<RemoteEnvelope>>>,
    next_remote_channel: AtomicU64,
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    authorized_origins: RwLock<HashSet<String>>,
    exit_requested: AtomicBool,
}

struct DesktopRuntimeInner {
    state: DesktopState,
    workspace: Option<WorkspaceLocation>,
    server: Option<RunningServer>,
    wsl: Option<WslConnection>,
    pending_wsl: Option<WslConnection>,
    remote_settings: Option<EmbeddedServer>,
}

struct StartedServer {
    server: RunningServer,
    navigation_url: Url,
}

enum StopServerOutcome {
    Stopped,
    Cancelled(RunningServer),
}

enum StopWslOutcome {
    Stopped,
    Cancelled(Box<WslConnection>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DesktopAction {
    StartDrag {},
    Minimize {},
    ToggleMaximize {},
    CloseWindow {},
    Quit {},
    OpenFolder {},
    OpenRecent { index: usize },
    OpenLogs {},
    DownloadLatest {},
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopShellState {
    is_maximized: bool,
    recent_workspaces: Vec<RecentWorkspace>,
    active_workspace: Option<WorkspaceLocation>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecentWorkspace {
    index: usize,
    label: String,
    target: String,
    path: String,
}

#[tauri::command]
async fn desktop_shell_state(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
) -> Result<DesktopShellState, String> {
    ensure_main_window(&window)?;
    let is_maximized = window.is_maximized().map_err(|error| error.to_string())?;
    let inner = runtime.inner.lock().await;
    let recent_workspaces = inner
        .state
        .recent_workspaces()
        .iter()
        .enumerate()
        .map(|(index, workspace)| RecentWorkspace {
            index,
            label: workspace_project_label(workspace),
            target: workspace.target_label(),
            path: workspace.display_path(),
        })
        .collect();
    Ok(DesktopShellState {
        is_maximized,
        recent_workspaces,
        active_workspace: inner.workspace.clone(),
    })
}

#[tauri::command]
async fn desktop_wsl_distributions(window: WebviewWindow) -> Result<Vec<WslDistribution>, String> {
    ensure_main_window(&window)?;
    wsl::list_distributions()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn desktop_wsl_probe(
    window: WebviewWindow,
    distro: String,
    user: String,
) -> Result<WslProbe, String> {
    ensure_main_window(&window)?;
    wsl::probe(&distro, &user)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn desktop_wsl_connect(
    app: AppHandle,
    window: WebviewWindow,
    distro: String,
    user: String,
    path: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    switch_wsl_workspace(&app, distro, user, path)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn desktop_wsl_prepare(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
    distro: String,
    user: String,
) -> Result<WslProbe, String> {
    ensure_main_window(&window)?;
    let _ = window.emit(
        "morrow-wsl-log",
        format!("Probing WSL distribution {distro}…"),
    );
    let probe = wsl::probe(&distro, &user)
        .await
        .map_err(|error| error.to_string())?;
    let _ = window.emit(
        "morrow-wsl-log",
        format!(
            "Detected {} as {} ({})",
            probe.distro, probe.user, probe.arch
        ),
    );
    let _ = window.emit(
        "morrow-wsl-log",
        "Checking the signed Runtime cache and remote installation…",
    );
    let connection = WslConnection::start(distro, user)
        .await
        .map_err(|error| error.to_string())?;
    let _ = window.emit(
        "morrow-wsl-log",
        "Runtime version and stdio protocol handshake verified.",
    );
    let old = runtime.inner.lock().await.pending_wsl.replace(connection);
    if let Some(old) = old {
        old.shutdown(true).await;
    }
    Ok(probe)
}

#[tauri::command]
async fn desktop_remote_request(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
    request: RemoteRequest,
) -> Result<RemoteResponse, String> {
    ensure_main_window(&window)?;
    let operation = runtime.operation.lock().await;
    let (client, settings) = {
        let inner = runtime.inner.lock().await;
        if let Some(connection) = inner.wsl.as_ref() {
            let settings = inner.remote_settings.clone().ok_or_else(|| {
                "Windows workspace settings are not ready; wait for WSL to reconnect".to_string()
            })?;
            (connection.request_client(), Some(settings))
        } else {
            let connection = inner
                .pending_wsl
                .as_ref()
                .ok_or_else(|| "no WSL workspace is connected".to_string())?;
            if !request_allowed_while_wsl_pending(&request) {
                return Err(
                    "WSL project is still connecting; finish opening a project first".to_string(),
                );
            }
            (connection.request_client(), None)
        }
    };
    drop(operation);
    match settings {
        Some(settings) => route_active_remote_request(client, settings, request).await,
        None => client
            .request(request)
            .await
            .map_err(|error| error.to_string()),
    }
}

#[tauri::command]
fn desktop_remote_subscribe(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
    on_event: Channel<RemoteEnvelope>,
) -> Result<u64, String> {
    ensure_main_window(&window)?;
    let subscription_id = runtime.next_remote_channel.fetch_add(1, Ordering::Relaxed);
    runtime
        .remote_channels
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(subscription_id, on_event);
    Ok(subscription_id)
}

#[tauri::command]
fn desktop_remote_unsubscribe(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
    subscription_id: u64,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    runtime
        .remote_channels
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&subscription_id);
    Ok(())
}

async fn route_active_remote_request(
    client: wsl::WslRequestClient,
    settings: EmbeddedServer,
    request: RemoteRequest,
) -> Result<RemoteResponse, String> {
    match request {
        RemoteRequest::Http { method, path, body } if path == "/api/status" => {
            let remote = client
                .request(RemoteRequest::Http {
                    method: method.clone(),
                    path: path.clone(),
                    body: body.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let local = settings.request(&method, &path, body).await?;
            merge_remote_status(remote, local)
        }
        RemoteRequest::Http {
            method: _,
            path,
            body,
        } if path == "/api/model-providers/discover" => {
            let spec = settings
                .prepare_remote_model_discovery(body.unwrap_or(serde_json::Value::Null))
                .await?;
            client
                .request(RemoteRequest::DiscoverModels { model: spec })
                .await
                .map_err(|error| error.to_string())
        }
        RemoteRequest::Http {
            method: _,
            path,
            body,
        } if path == "/api/mcp-servers/test" => {
            let server = settings
                .prepare_remote_mcp_test(body.unwrap_or(serde_json::Value::Null))
                .await?;
            client
                .request(RemoteRequest::InspectMcp {
                    server: Box::new(server),
                })
                .await
                .map_err(|error| error.to_string())
        }
        RemoteRequest::Http { method, path, body } if is_windows_owned_settings_path(&path) => {
            let response = settings.request(&method, &path, body).await?;
            Ok(RemoteResponse::Http(agent_protocol::RemoteHttpResponse {
                status: response.status,
                body: response.body,
            }))
        }
        RemoteRequest::SessionMessage { session, message }
            if message.get("type").and_then(serde_json::Value::as_str) == Some("start_turn") =>
        {
            let turn = settings.prepare_remote_turn(&session, message).await?;
            client
                .request(RemoteRequest::StartTurn {
                    turn: Box::new(turn),
                })
                .await
                .map_err(|error| error.to_string())
        }
        RemoteRequest::SessionMessage { session, message }
            if matches!(
                message.get("type").and_then(serde_json::Value::as_str),
                Some("spawn_subagent" | "send_subagent")
            ) =>
        {
            let command = settings
                .prepare_remote_subagent_message(&session, message)
                .await?;
            client
                .request(RemoteRequest::SubagentMessage {
                    command: Box::new(command),
                })
                .await
                .map_err(|error| error.to_string())
        }
        request => client
            .request(request)
            .await
            .map_err(|error| error.to_string()),
    }
}

fn request_allowed_while_wsl_pending(request: &RemoteRequest) -> bool {
    matches!(
        request,
        RemoteRequest::Ping
            | RemoteRequest::Activity
            | RemoteRequest::Environment
            | RemoteRequest::ListDirectory { .. }
    )
}

fn is_windows_owned_settings_path(path: &str) -> bool {
    path == "/api/model-settings"
        || path == "/api/model-default"
        || path.starts_with("/api/model-providers")
        || path == "/api/mcp-settings"
        || path.starts_with("/api/mcp-servers")
        || path == "/api/commands"
        || path.starts_with("/api/commands/")
        || path == "/api/subagent-settings"
        || path.starts_with("/api/subagent-settings/")
        || path == "/api/subagents"
        || path.starts_with("/api/subagents/")
        || (path.starts_with("/api/sessions/") && path.ends_with("/model-selection"))
}

fn merge_remote_status(
    remote: RemoteResponse,
    local: agent_server::EmbeddedHttpResponse,
) -> Result<RemoteResponse, String> {
    let RemoteResponse::Http(mut remote) = remote else {
        return Err("remote workspace returned an invalid status response".to_string());
    };
    let Some(serde_json::Value::Object(local)) = local.body else {
        return Ok(RemoteResponse::Http(remote));
    };
    let Some(serde_json::Value::Object(remote_body)) = remote.body.as_mut() else {
        return Ok(RemoteResponse::Http(remote));
    };
    for key in [
        "model_ready",
        "model_store_path",
        "mcp_store_path",
        "command_store_path",
        "subagent_store_path",
    ] {
        if let Some(value) = local.get(key) {
            remote_body.insert(key.to_string(), value.clone());
        }
    }
    Ok(RemoteResponse::Http(remote))
}

#[tauri::command]
async fn desktop_action(
    app: AppHandle,
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
    action: DesktopAction,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    match action {
        DesktopAction::StartDrag {} => window.start_dragging().map_err(|error| error.to_string()),
        DesktopAction::Minimize {} => window.minimize().map_err(|error| error.to_string()),
        DesktopAction::ToggleMaximize {} => {
            if window.is_maximized().map_err(|error| error.to_string())? {
                window.unmaximize().map_err(|error| error.to_string())
            } else {
                window.maximize().map_err(|error| error.to_string())
            }
        }
        DesktopAction::CloseWindow {} => window.close().map_err(|error| error.to_string()),
        DesktopAction::Quit {} => {
            request_exit(app);
            Ok(())
        }
        DesktopAction::OpenFolder {} => {
            request_workspace_picker(&app, false);
            Ok(())
        }
        DesktopAction::OpenRecent { index } => {
            let workspace = {
                let inner = runtime.inner.lock().await;
                recent_workspace_at(&inner.state, index)?
            };
            spawn_location_switch(app, workspace, false);
            Ok(())
        }
        DesktopAction::OpenLogs {} => {
            open_logs_directory(&app);
            Ok(())
        }
        DesktopAction::DownloadLatest {} => app
            .opener()
            .open_url(RELEASES_URL, None::<&str>)
            .map_err(|error| error.to_string()),
    }
}

fn ensure_main_window(window: &WebviewWindow) -> Result<(), String> {
    ensure_main_window_label(window.label())
}

fn ensure_main_window_label(label: &str) -> Result<(), String> {
    if label == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err("desktop shell commands are only available to the main window".to_string())
    }
}

fn recent_workspace_at(state: &DesktopState, index: usize) -> Result<WorkspaceLocation, String> {
    state
        .recent_workspaces()
        .get(index)
        .cloned()
        .ok_or_else(|| format!("recent workspace index {index} is out of bounds"))
}

pub fn run() {
    let log_plugin = tauri_plugin_log::Builder::new()
        .targets([Target::new(TargetKind::LogDir {
            file_name: Some("morrow-desktop".into()),
        })])
        .rotation_strategy(RotationStrategy::KeepSome(5))
        .max_file_size(2_000_000)
        .level(log::LevelFilter::Info)
        .build();
    let window_state_plugin = tauri_plugin_window_state::Builder::new()
        .with_state_flags(
            tauri_plugin_window_state::StateFlags::SIZE
                | tauri_plugin_window_state::StateFlags::POSITION
                | tauri_plugin_window_state::StateFlags::MAXIMIZED,
        )
        .build();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            focus_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(log_plugin)
        .plugin(window_state_plugin)
        .plugin(tauri_plugin_opener::init())
        .setup(|app| setup_app(app).map_err(Into::into))
        .invoke_handler(tauri::generate_handler![
            desktop_shell_state,
            desktop_action,
            desktop_wsl_distributions,
            desktop_wsl_probe,
            desktop_wsl_prepare,
            desktop_wsl_connect,
            desktop_remote_request,
            desktop_remote_subscribe,
            desktop_remote_unsubscribe
        ]);
    #[cfg(target_os = "macos")]
    let builder = builder.on_menu_event(handle_menu_event);

    let app = builder
        .build(tauri::generate_context!())
        .expect("failed to build Morrow desktop");

    app.run(handle_run_event);
}

fn setup_app(app: &mut tauri::App) -> Result<(), DesktopError> {
    let home = dirs::home_dir().ok_or(DesktopError::HomeDirectoryNotFound)?;
    let state_path = home.join(".morrow").join("desktop.json");
    let default_workspace = default_workspace_path(&home);
    std::fs::create_dir_all(&default_workspace).map_err(|error| {
        DesktopError::BackendConfiguration(format!(
            "failed to create default workspace {}: {error}",
            default_workspace.display()
        ))
    })?;
    let mut state = match DesktopState::load(&state_path) {
        Ok(state) => state,
        Err(error) => {
            log::warn!("ignoring unreadable desktop state: {error}");
            DesktopState::default()
        }
    };
    if state.prune_invalid_workspaces()
        && let Err(error) = state.save(&state_path)
    {
        log::warn!("failed to persist pruned desktop state: {error}");
    }
    let initial_workspace = state
        .last_workspace()
        .cloned()
        .unwrap_or(WorkspaceLocation::Local {
            path: default_workspace,
        });
    let bootstrap_token = generate_bootstrap_token()?;

    app.manage(DesktopRuntime {
        home,
        state_path,
        bootstrap_token,
        inner: Mutex::new(DesktopRuntimeInner {
            state: state.clone(),
            workspace: None,
            server: None,
            wsl: None,
            pending_wsl: None,
            remote_settings: None,
        }),
        operation: Mutex::new(()),
        navigation_origin: RwLock::new(None),
        app_url: RwLock::new(None),
        remote_channels: StdMutex::new(HashMap::new()),
        next_remote_channel: AtomicU64::new(1),
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
        authorized_origins: RwLock::new(HashSet::new()),
        exit_requested: AtomicBool::new(false),
    });
    #[cfg(target_os = "macos")]
    replace_menu(app.handle(), &state)?;
    log::info!("Morrow desktop started");

    show_connection_window(app.handle())?;
    spawn_location_switch(app.handle().clone(), initial_workspace, true);
    Ok(())
}

fn default_workspace_path(home: &Path) -> PathBuf {
    home.join(".morrow").join("workspaces").join("default")
}

fn generate_bootstrap_token() -> Result<String, DesktopError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| DesktopError::Random(error.to_string()))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(token)
}

fn request_workspace_picker(app: &AppHandle, exit_on_cancel: bool) {
    let handle = app.clone();
    app.dialog()
        .file()
        .set_title("Open a Morrow workspace")
        .pick_folder(move |selection| {
            let Some(workspace) = selection.and_then(|path| path.into_path().ok()) else {
                if exit_on_cancel {
                    handle.exit(0);
                }
                return;
            };
            spawn_workspace_switch(handle, workspace, exit_on_cancel);
        });
}

fn spawn_location_switch(app: AppHandle, workspace: WorkspaceLocation, exit_on_error: bool) {
    match workspace {
        WorkspaceLocation::Local { path } => spawn_workspace_switch(app, path, exit_on_error),
        WorkspaceLocation::Wsl { distro, user, path } => {
            tauri::async_runtime::spawn(async move {
                if let Err(error) = switch_wsl_workspace(&app, distro, user, path).await {
                    log::error!("failed to restore WSL workspace: {error}");
                    if exit_on_error {
                        let default_workspace = {
                            let runtime = app.state::<DesktopRuntime>();
                            default_workspace_path(&runtime.home)
                        };
                        if let Err(fallback_error) = switch_workspace(&app, default_workspace).await
                        {
                            show_error(
                                &app,
                                "Morrow could not open a workspace",
                                &format!(
                                    "WSL reconnect failed: {error}\n\nDefault workspace failed: {fallback_error}"
                                ),
                            );
                        }
                    } else {
                        show_error(&app, "Morrow could not connect to WSL", &error.to_string());
                    }
                }
            });
        }
    }
}

fn spawn_workspace_switch(app: AppHandle, workspace: PathBuf, exit_on_error: bool) {
    tauri::async_runtime::spawn(async move {
        if let Err(error) = switch_workspace(&app, workspace).await {
            log::error!("failed to switch workspace ({})", error.log_category());
            if exit_on_error && app.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
                show_error_and_exit(
                    &app,
                    "Morrow could not open the workspace",
                    &error.to_string(),
                );
            } else {
                show_error(
                    &app,
                    "Morrow could not open the workspace",
                    &error.to_string(),
                );
            }
        }
    });
}

async fn switch_workspace(app: &AppHandle, workspace: PathBuf) -> Result<(), DesktopError> {
    let runtime = app.state::<DesktopRuntime>();
    let _operation = runtime.operation.lock().await;
    if runtime.exit_requested.load(Ordering::Acquire) {
        return Ok(());
    }

    let workspace = DesktopState::validate_workspace(&workspace)?;
    let location = WorkspaceLocation::Local {
        path: workspace.clone(),
    };
    {
        let inner = runtime.inner.lock().await;
        if inner.workspace.as_ref() == Some(&location) {
            focus_main_window(app);
            return Ok(());
        }
    }
    let options = prepare_server_options(&runtime.home, &workspace)?;

    let (old_server, old_wsl, old_workspace) = {
        let mut inner = runtime.inner.lock().await;
        (
            inner.server.take(),
            inner.wsl.take(),
            inner.workspace.clone(),
        )
    };
    if let Some(server) = old_server {
        match stop_server_with_confirmation(app, server, "switch workspaces").await? {
            StopServerOutcome::Stopped => {}
            StopServerOutcome::Cancelled(server) => {
                runtime.inner.lock().await.server = Some(server);
                return Ok(());
            }
        }
    }
    if let Some(connection) = old_wsl {
        match stop_wsl_with_confirmation(app, connection, "switch workspaces").await? {
            StopWslOutcome::Stopped => {}
            StopWslOutcome::Cancelled(connection) => {
                runtime.inner.lock().await.wsl = Some(*connection);
                return Ok(());
            }
        }
    }

    let started = match launch_server(options, &runtime.bootstrap_token).await {
        Ok(started) => started,
        Err(error) => {
            if let Some(WorkspaceLocation::Local {
                path: old_workspace,
            }) = old_workspace
            {
                match prepare_server_options(&runtime.home, &old_workspace) {
                    Ok(options) => match launch_server(options, &runtime.bootstrap_token).await {
                        Ok(recovered) => {
                            let recovered_url = recovered.navigation_url.clone();
                            if let Err(recovery_error) =
                                authorize_navigation_origin(app, &runtime, &recovered_url)
                            {
                                log::error!(
                                    "failed to authorize the previous workspace ({})",
                                    recovery_error.log_category()
                                );
                                return Err(recovery_error);
                            }
                            runtime.inner.lock().await.server = Some(recovered.server);
                            set_navigation_origin(&runtime, &recovered_url);
                            if let Err(recovery_error) =
                                show_workspace_window(app, &old_workspace, recovered_url)
                            {
                                log::error!(
                                    "failed to restore the previous window ({})",
                                    recovery_error.log_category()
                                );
                            }
                        }
                        Err(recovery_error) => {
                            log::error!(
                                "failed to restart the previous workspace ({})",
                                recovery_error.log_category()
                            );
                        }
                    },
                    Err(recovery_error) => {
                        log::error!(
                            "failed to reload the previous workspace ({})",
                            recovery_error.log_category()
                        );
                    }
                }
            }
            let has_runtime = {
                let mut inner = runtime.inner.lock().await;
                let has_runtime = inner.server.is_some() || inner.wsl.is_some();
                if !has_runtime {
                    inner.workspace = None;
                    inner.remote_settings = None;
                }
                has_runtime
            };
            if !has_runtime {
                show_connection_shell(app, &runtime)?;
            }
            return Err(error);
        }
    };

    authorize_navigation_origin(app, &runtime, &started.navigation_url)?;
    let mut next_state = {
        let inner = runtime.inner.lock().await;
        inner.state.clone()
    };
    next_state.record_local_workspace(&workspace)?;
    let navigation_url = started.navigation_url.clone();
    {
        let mut inner = runtime.inner.lock().await;
        inner.state = next_state.clone();
        inner.workspace = Some(location);
        inner.server = Some(started.server);
        inner.wsl = None;
        inner.remote_settings = None;
    }
    if let Err(error) = next_state.save(&runtime.state_path) {
        log::warn!("failed to save desktop state: {error}");
        show_error(
            app,
            "Morrow could not save desktop state",
            "The workspace is open, but it may not be restored on the next launch.",
        );
    }
    set_navigation_origin(&runtime, &navigation_url);
    show_workspace_window(app, &workspace, navigation_url)?;
    #[cfg(target_os = "macos")]
    if let Err(error) = replace_menu(app, &next_state) {
        log::error!(
            "failed to refresh the application menu ({})",
            error.log_category()
        );
        show_error(
            app,
            "Morrow could not refresh the application menu",
            "The workspace is open. Restart Morrow if Open Recent is out of date.",
        );
    }
    log::info!("opened workspace {}", workspace.display());
    Ok(())
}

async fn switch_wsl_workspace(
    app: &AppHandle,
    distro: String,
    user: String,
    path: String,
) -> Result<(), DesktopError> {
    let runtime = app.state::<DesktopRuntime>();
    let _operation = runtime.operation.lock().await;
    if runtime.exit_requested.load(Ordering::Acquire) {
        return Ok(());
    }
    let requested = WorkspaceLocation::Wsl {
        distro: distro.clone(),
        user: user.clone(),
        path: path.clone(),
    };
    {
        let inner = runtime.inner.lock().await;
        if inner.workspace.as_ref() == Some(&requested) && inner.wsl.is_some() {
            focus_main_window(app);
            return Ok(());
        }
    }

    let (old_server, old_wsl) = {
        let mut inner = runtime.inner.lock().await;
        (inner.server.take(), inner.wsl.take())
    };
    let reload_app_shell = old_wsl.is_some();
    if let Some(server) = old_server {
        match stop_server_with_confirmation(app, server, "connect to WSL").await? {
            StopServerOutcome::Stopped => {}
            StopServerOutcome::Cancelled(server) => {
                runtime.inner.lock().await.server = Some(server);
                return Ok(());
            }
        }
    }
    if let Some(connection) = old_wsl {
        match stop_wsl_with_confirmation(app, connection, "switch WSL workspaces").await? {
            StopWslOutcome::Stopped => {}
            StopWslOutcome::Cancelled(connection) => {
                runtime.inner.lock().await.wsl = Some(*connection);
                return Ok(());
            }
        }
    }

    let pending = runtime.inner.lock().await.pending_wsl.take();
    let connection_result: Result<_, DesktopError> = async {
        let mut connection = match pending {
            Some(connection)
                if connection.distro == distro && (user.is_empty() || connection.user == user) =>
            {
                connection
            }
            Some(connection) => {
                connection.shutdown(true).await;
                WslConnection::start(distro, user)
                    .await
                    .map_err(|error| DesktopError::Wsl(error.to_string()))?
            }
            None => WslConnection::start(distro, user)
                .await
                .map_err(|error| DesktopError::Wsl(error.to_string()))?,
        };
        let configuration = connection
            .open_workspace(path)
            .await
            .map_err(|error| DesktopError::Wsl(error.to_string()))?;
        Ok((connection, configuration))
    }
    .await;
    let (connection, workspace_configuration) = match connection_result {
        Ok(connection) => connection,
        Err(error) => {
            let mut inner = runtime.inner.lock().await;
            inner.workspace = None;
            inner.remote_settings = None;
            drop(inner);
            show_connection_shell(app, &runtime)?;
            return Err(error);
        }
    };
    let location = WorkspaceLocation::Wsl {
        distro: connection.distro.clone(),
        user: connection.user.clone(),
        path: connection.workspace.clone(),
    };
    let remote_settings =
        match prepare_remote_settings(&runtime.home, &location, workspace_configuration) {
            Ok(settings) => settings,
            Err(error) => {
                let mut inner = runtime.inner.lock().await;
                inner.workspace = None;
                inner.remote_settings = None;
                drop(inner);
                show_connection_shell(app, &runtime)?;
                return Err(error);
            }
        };
    let mut events = connection.subscribe_events();
    let mut host_closed = connection.subscribe_closed();

    let mut next_state = runtime.inner.lock().await.state.clone();
    next_state.record_workspace(location.clone())?;
    {
        let mut inner = runtime.inner.lock().await;
        inner.state = next_state.clone();
        inner.workspace = Some(location.clone());
        inner.server = None;
        inner.wsl = Some(connection);
        inner.remote_settings = Some(remote_settings);
    }
    if let Err(error) = next_state.save(&runtime.state_path) {
        log::warn!("failed to save WSL workspace state: {error}");
    }
    show_remote_workspace_window(app, &runtime, &location, reload_app_shell)?;
    #[cfg(target_os = "macos")]
    replace_menu(app, &next_state)?;
    let event_app = app.clone();
    let reconnect_location = location.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => {
                        let worker_exited = matches!(
                            &event.message,
                            RemoteMessage::Event(RemoteEvent::WorkerExited { .. })
                        );
                        send_remote_event(&event_app, event);
                        if worker_exited {
                            match reconnect_wsl_worker(&event_app, &reconnect_location).await {
                                Ok(Some(channel_id)) => {
                                    emit_workspace_reconnected(&event_app, channel_id);
                                }
                                Ok(None) => break,
                                Err(error) => {
                                    log::warn!(
                                        "failed to reconnect WSL workspace worker; restarting host: {error}"
                                    );
                                    match reconnect_wsl_host(&event_app, &reconnect_location).await {
                                        Ok(Some(reconnected)) => {
                                            events = reconnected.events;
                                            host_closed = reconnected.closed;
                                            emit_workspace_reconnected(
                                                &event_app,
                                                reconnected.channel_id,
                                            );
                                        }
                                        Ok(None) => break,
                                        Err(error) => {
                                            log::error!("failed to reconnect WSL host: {error}");
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        log::warn!("dropped {skipped} remote workspace events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        match reconnect_wsl_host(&event_app, &reconnect_location).await {
                            Ok(Some(reconnected)) => {
                                events = reconnected.events;
                                host_closed = reconnected.closed;
                                emit_workspace_reconnected(&event_app, reconnected.channel_id);
                            }
                            Ok(None) => break,
                            Err(error) => {
                                log::error!("failed to reconnect WSL host: {error}");
                                break;
                            }
                        }
                    }
                },
                _ = host_closed.changed() => {
                    match reconnect_wsl_host(&event_app, &reconnect_location).await {
                        Ok(Some(reconnected)) => {
                            events = reconnected.events;
                            host_closed = reconnected.closed;
                            emit_workspace_reconnected(&event_app, reconnected.channel_id);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            log::error!("failed to reconnect WSL host: {error}");
                            break;
                        }
                    }
                }
            }
        }
    });
    log::info!("connected workspace {}", location.display_path());
    Ok(())
}

fn emit_workspace_reconnected(app: &AppHandle, channel_id: u32) {
    let reconnected = RemoteEnvelope::new(
        channel_id,
        format!("workspace-reconnected-{channel_id}"),
        RemoteMessage::Event(RemoteEvent::WorkspaceReconnected { channel_id }),
    );
    send_remote_event(app, reconnected);
}

fn send_remote_event(app: &AppHandle, event: RemoteEnvelope) {
    let Some(runtime) = app.try_state::<DesktopRuntime>() else {
        return;
    };
    runtime
        .remote_channels
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|_, channel| channel.send(event.clone()).is_ok());
}

struct ReconnectedWslHost {
    events: tokio::sync::broadcast::Receiver<RemoteEnvelope>,
    closed: tokio::sync::watch::Receiver<bool>,
    channel_id: u32,
}

async fn reconnect_wsl_host(
    app: &AppHandle,
    location: &WorkspaceLocation,
) -> Result<Option<ReconnectedWslHost>, String> {
    let runtime = app.state::<DesktopRuntime>();
    let _operation = runtime.operation.lock().await;
    if runtime.exit_requested.load(Ordering::Acquire) {
        return Ok(None);
    }
    let (distro, user, path) = match location {
        WorkspaceLocation::Wsl { distro, user, path } => {
            (distro.clone(), user.clone(), path.clone())
        }
        WorkspaceLocation::Local { .. } => return Ok(None),
    };
    let old = {
        let mut inner = runtime.inner.lock().await;
        if inner.workspace.as_ref() != Some(location) {
            return Ok(None);
        }
        inner.remote_settings = None;
        inner.wsl.take()
    };
    if let Some(old) = old {
        old.shutdown(true).await;
    }
    let mut connection = WslConnection::start(distro, user)
        .await
        .map_err(|error| error.to_string())?;
    let configuration = connection
        .open_workspace(path)
        .await
        .map_err(|error| error.to_string())?;
    let settings = prepare_remote_settings(&runtime.home, location, configuration)
        .map_err(|error| error.to_string())?;
    let reconnected = ReconnectedWslHost {
        events: connection.subscribe_events(),
        closed: connection.subscribe_closed(),
        channel_id: connection.channel_id,
    };
    let mut inner = runtime.inner.lock().await;
    if inner.workspace.as_ref() != Some(location) || runtime.exit_requested.load(Ordering::Acquire)
    {
        drop(inner);
        connection.shutdown(true).await;
        return Ok(None);
    }
    inner.remote_settings = Some(settings);
    inner.wsl = Some(connection);
    Ok(Some(reconnected))
}

async fn reconnect_wsl_worker(
    app: &AppHandle,
    location: &WorkspaceLocation,
) -> Result<Option<u32>, String> {
    let runtime = app.state::<DesktopRuntime>();
    let _operation = runtime.operation.lock().await;
    if runtime.exit_requested.load(Ordering::Acquire) {
        return Ok(None);
    }
    let mut inner = runtime.inner.lock().await;
    if inner.workspace.as_ref() != Some(location) {
        return Ok(None);
    }
    let connection = inner
        .wsl
        .as_mut()
        .ok_or_else(|| "WSL connection disappeared during reconnect".to_string())?;
    let configuration = connection
        .open_workspace(location.display_path())
        .await
        .map_err(|error| error.to_string())?;
    let channel_id = connection.channel_id;
    inner.remote_settings = Some(
        prepare_remote_settings(&runtime.home, location, configuration)
            .map_err(|error| error.to_string())?,
    );
    Ok(Some(channel_id))
}

fn prepare_server_options(home: &Path, workspace: &Path) -> Result<ServerOptions, DesktopError> {
    let loaded = load_server_config_for_workspace(None, workspace)?;
    let port = if cfg!(debug_assertions) { 3000 } else { 0 };
    server_options_from_loaded_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
        workspace.to_path_buf(),
        home,
        loaded,
        "default".to_string(),
    )
    .map_err(|error| DesktopError::BackendConfiguration(error.to_string()))
}

fn prepare_remote_settings(
    home: &Path,
    location: &WorkspaceLocation,
    configuration: RemoteWorkspaceConfiguration,
) -> Result<EmbeddedServer, DesktopError> {
    let scope_bytes = serde_json::to_vec(location)
        .map_err(|error| DesktopError::BackendConfiguration(error.to_string()))?;
    let scope = format!("{:x}", Sha256::digest(scope_bytes));
    let morrow_home = home.join(".morrow");
    let workspace_scope = morrow_home.join("remote-workspaces").join(scope);
    std::fs::create_dir_all(&workspace_scope)
        .map_err(|error| DesktopError::BackendConfiguration(error.to_string()))?;
    let fallback_model = configuration.fallback_model.map(FallbackModel::remote);
    let fallback_mcp_servers = configuration
        .fallback_mcp_servers
        .into_iter()
        .map(|server| McpServerConfig {
            name: server.name,
            transport: match server.transport {
                RemoteMcpTransport::Stdio => McpTransport::Stdio,
                RemoteMcpTransport::Http => McpTransport::Http,
            },
            command: server.command,
            args: server.args,
            env: server
                .env_keys
                .into_iter()
                .map(|key| (key, String::new()))
                .collect(),
            cwd: server.cwd.map(PathBuf::from),
            url: server.url,
            http_headers: server
                .http_header_keys
                .into_iter()
                .map(|key| (key, String::new()))
                .collect(),
            enabled: server.enabled,
            startup_timeout_sec: server.startup_timeout_sec,
            tool_timeout_sec: server.tool_timeout_sec,
        })
        .collect();
    let options = ServerOptions {
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        fallback_model,
        model_store_path: morrow_home.join("web-models.json"),
        mcp_store_path: morrow_home.join("web-mcp.json"),
        command_store_path: morrow_home.join("commands"),
        subagent_store_path: morrow_home.join("subagents.json"),
        system_prompt: String::new(),
        context_config: ContextConfig {
            auto_compact: true,
            auto_compact_threshold: 0.835,
            retain_recent_turns: 6,
            summary_target_tokens: 12_000,
            compact_max_retries: 2,
        },
        workspace_root: workspace_scope,
        workspace_location: location.clone(),
        config_path: None,
        config_diagnostics: Vec::new(),
        permissions: PermissionProfile::for_mode(agent_server::DEFAULT_WEB_PERMISSION_MODE),
        mcp_servers: fallback_mcp_servers,
        default_session_name: "default".to_string(),
    };
    EmbeddedServer::new(options)
        .map_err(|error| DesktopError::BackendConfiguration(error.to_string()))
}

async fn launch_server(
    options: ServerOptions,
    bootstrap_token: &str,
) -> Result<StartedServer, DesktopError> {
    let access_policy = if cfg!(debug_assertions) {
        ServerAccessPolicy::Browser
    } else {
        ServerAccessPolicy::desktop(bootstrap_token)
    };
    let server = spawn_local(options, access_policy).await?;
    let navigation_url = if cfg!(debug_assertions) {
        parse_url(VITE_DEV_URL)?
    } else {
        parse_url(&format!(
            "{}/?desktop_bootstrap={bootstrap_token}",
            server.base_url()
        ))?
    };
    Ok(StartedServer {
        server,
        navigation_url,
    })
}

async fn stop_server_with_confirmation(
    app: &AppHandle,
    mut server: RunningServer,
    action: &str,
) -> Result<StopServerOutcome, DesktopError> {
    let activity = server.activity().await;
    let mut policy = ShutdownPolicy::RequireIdle;
    if !activity.is_idle() {
        if !confirm_cancel_running_turns(app, activity.running_turns, action).await {
            return Ok(StopServerOutcome::Cancelled(server));
        }
        policy = ShutdownPolicy::CancelRunning {
            timeout: SERVER_SHUTDOWN_TIMEOUT,
        };
    }

    match server.shutdown(policy).await {
        Ok(()) => Ok(StopServerOutcome::Stopped),
        Err(agent_server::ServerError::RunningTurns(count)) => {
            if !confirm_cancel_running_turns(app, count, action).await {
                return Ok(StopServerOutcome::Cancelled(server));
            }
            server
                .shutdown(ShutdownPolicy::CancelRunning {
                    timeout: SERVER_SHUTDOWN_TIMEOUT,
                })
                .await?;
            Ok(StopServerOutcome::Stopped)
        }
        Err(error) => Err(error.into()),
    }
}

async fn stop_wsl_with_confirmation(
    app: &AppHandle,
    connection: WslConnection,
    action: &str,
) -> Result<StopWslOutcome, DesktopError> {
    let running_turns = match connection.request(RemoteRequest::Activity).await {
        Ok(RemoteResponse::Activity(activity)) => activity.running_turns,
        Ok(_) => 0,
        Err(error) => {
            log::warn!("WSL activity check failed during shutdown: {error}");
            connection.shutdown(true).await;
            return Ok(StopWslOutcome::Stopped);
        }
    };
    if running_turns > 0 && !confirm_cancel_running_turns(app, running_turns, action).await {
        return Ok(StopWslOutcome::Cancelled(Box::new(connection)));
    }
    connection.shutdown(running_turns > 0).await;
    Ok(StopWslOutcome::Stopped)
}

async fn confirm_cancel_running_turns(app: &AppHandle, count: usize, action: &str) -> bool {
    let noun = if count == 1 { "turn" } else { "turns" };
    let message =
        format!("Morrow has {count} running {noun}. Cancel the running work and {action}?");
    let (sender, receiver) = oneshot::channel();
    app.dialog()
        .message(message)
        .title("Running work")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Cancel work and continue".to_string(),
            "Keep working".to_string(),
        ))
        .show(move |confirmed| {
            let _ = sender.send(confirmed);
        });
    receiver.await.unwrap_or(false)
}

fn show_connection_window(app: &AppHandle) -> Result<(), DesktopError> {
    if app.get_webview_window(MAIN_WINDOW_LABEL).is_some() {
        focus_main_window(app);
        return Ok(());
    }
    let app_url = desktop_app_url()?;
    let webview_url = if cfg!(debug_assertions) {
        WebviewUrl::External(app_url.clone())
    } else {
        WebviewUrl::App("index.html".into())
    };
    let builder = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, webview_url)
        .title("Morrow")
        .inner_size(1280.0, 800.0)
        .min_inner_size(960.0, 640.0)
        .devtools(cfg!(debug_assertions));
    #[cfg(target_os = "windows")]
    let builder = builder.decorations(false).shadow(true);
    #[cfg(target_os = "linux")]
    let builder = builder.decorations(false).shadow(true).transparent(true);
    #[cfg(target_os = "macos")]
    let builder = builder
        .decorations(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(14.0, 13.0));
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    let builder = builder.initialization_script(desktop_platform_initialization_script());
    let window = builder.build()?;
    install_close_handler(&window);
    let runtime = app.state::<DesktopRuntime>();
    *runtime
        .app_url
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(app_url);
    Ok(())
}

fn desktop_app_url() -> Result<Url, DesktopError> {
    desktop_app_url_for(
        cfg!(debug_assertions).then_some(VITE_DEV_URL),
        cfg!(target_os = "windows"),
    )
}

fn desktop_app_url_for(dev_url: Option<&str>, windows: bool) -> Result<Url, DesktopError> {
    let url = match dev_url {
        Some(dev_url) => format!("{dev_url}?desktop_connect=1"),
        None if windows => "http://tauri.localhost".to_string(),
        None => "tauri://localhost".to_string(),
    };
    parse_url(&url)
}

fn show_remote_workspace_window(
    app: &AppHandle,
    runtime: &DesktopRuntime,
    workspace: &WorkspaceLocation,
    reload_app_shell: bool,
) -> Result<(), DesktopError> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| DesktopError::BackendConfiguration("main window is unavailable".into()))?;
    let app_url = runtime
        .app_url
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or_else(|| {
            DesktopError::BackendConfiguration("desktop app URL is unavailable".into())
        })?;
    window.set_title(&workspace_location_title(workspace))?;
    navigate_app_shell(&window, app_url, reload_app_shell)?;
    window.show()?;
    let _ = window.unminimize();
    window.set_focus()?;
    Ok(())
}

fn show_connection_shell(app: &AppHandle, runtime: &DesktopRuntime) -> Result<(), DesktopError> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| DesktopError::BackendConfiguration("main window is unavailable".into()))?;
    let app_url = runtime
        .app_url
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or_else(|| {
            DesktopError::BackendConfiguration("desktop app URL is unavailable".into())
        })?;
    window.set_title("Morrow")?;
    navigate_app_shell(&window, app_url, true)?;
    window.show()?;
    let _ = window.unminimize();
    window.set_focus()?;
    Ok(())
}

fn navigate_app_shell(
    window: &WebviewWindow,
    url: Url,
    reload_current: bool,
) -> Result<(), DesktopError> {
    match app_shell_navigation(window.url().ok().as_ref(), &url, reload_current) {
        AppShellNavigation::None => {}
        AppShellNavigation::Reload => window.reload()?,
        AppShellNavigation::Navigate => window.navigate(url)?,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppShellNavigation {
    None,
    Reload,
    Navigate,
}

fn app_shell_navigation(
    current: Option<&Url>,
    target: &Url,
    reload_current: bool,
) -> AppShellNavigation {
    if current != Some(target) {
        AppShellNavigation::Navigate
    } else if reload_current {
        AppShellNavigation::Reload
    } else {
        AppShellNavigation::None
    }
}

fn show_workspace_window(
    app: &AppHandle,
    workspace: &Path,
    navigation_url: Url,
) -> Result<(), DesktopError> {
    let title = workspace_title(workspace);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        window.set_title(&title)?;
        window.navigate(navigation_url)?;
        window.show()?;
        let _ = window.unminimize();
        window.set_focus()?;
        return Ok(());
    }

    let navigation_handle = app.clone();
    let new_window_handle = app.clone();
    let builder =
        WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(navigation_url))
            .title(title)
            .inner_size(1280.0, 800.0)
            .min_inner_size(960.0, 640.0)
            .devtools(cfg!(debug_assertions))
            .on_navigation(move |url| handle_navigation(&navigation_handle, url))
            .on_new_window(move |url, _features| {
                open_external_url(&new_window_handle, &url);
                NewWindowResponse::Deny
            });
    #[cfg(target_os = "windows")]
    let builder = builder.decorations(false).shadow(true);
    #[cfg(target_os = "linux")]
    let builder = builder.decorations(false).shadow(true).transparent(true);
    #[cfg(target_os = "macos")]
    let builder = builder
        .decorations(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(14.0, 13.0));
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    let builder = builder.initialization_script(desktop_platform_initialization_script());

    let window = builder.build()?;
    install_close_handler(&window);
    Ok(())
}

#[cfg(target_os = "windows")]
fn desktop_platform_initialization_script() -> &'static str {
    r#"Object.defineProperty(window, "__MORROW_DESKTOP__", {
  value: Object.freeze({ platform: "windows" }),
  writable: false,
  configurable: false,
  enumerable: true
});"#
}

#[cfg(target_os = "macos")]
fn desktop_platform_initialization_script() -> &'static str {
    r#"Object.defineProperty(window, "__MORROW_DESKTOP__", {
  value: Object.freeze({ platform: "macos" }),
  writable: false,
  configurable: false,
  enumerable: true
});"#
}

#[cfg(target_os = "linux")]
fn desktop_platform_initialization_script() -> &'static str {
    r#"Object.defineProperty(window, "__MORROW_DESKTOP__", {
  value: Object.freeze({ platform: "linux" }),
  writable: false,
  configurable: false,
  enumerable: true
});"#
}

fn set_navigation_origin(runtime: &DesktopRuntime, url: &Url) {
    let mut origin = runtime
        .navigation_origin
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *origin = Some(url.origin().ascii_serialization());
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn authorize_navigation_origin(
    app: &AppHandle,
    runtime: &DesktopRuntime,
    url: &Url,
) -> Result<(), DesktopError> {
    let origin = loopback_http_origin(url)?;
    let mut authorized = runtime
        .authorized_origins
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if authorized.contains(&origin) {
        return Ok(());
    }

    let identifier = format!("desktop-shell-origin-{}", authorized.len());
    app.add_capability(desktop_remote_capability(&identifier, &origin))?;
    authorized.insert(origin);
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn authorize_navigation_origin(
    _app: &AppHandle,
    _runtime: &DesktopRuntime,
    url: &Url,
) -> Result<(), DesktopError> {
    loopback_http_origin(url).map(|_| ())
}

fn loopback_http_origin(url: &Url) -> Result<String, DesktopError> {
    let is_loopback = url
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback());
    if url.scheme() != "http" || !is_loopback {
        return Err(DesktopError::InvalidNavigationOrigin(
            url.origin().ascii_serialization(),
        ));
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux", test))]
fn desktop_remote_capability(identifier: &str, origin: &str) -> tauri::ipc::CapabilityBuilder {
    tauri::ipc::CapabilityBuilder::new(identifier)
        .remote(format!("{origin}/*"))
        .local(false)
        .window(MAIN_WINDOW_LABEL)
        .permission(DESKTOP_SHELL_STATE_PERMISSION)
        .permission(DESKTOP_ACTION_PERMISSION)
        .permission(DESKTOP_WSL_DISTRIBUTIONS_PERMISSION)
        .permission(DESKTOP_WSL_PROBE_PERMISSION)
        .permission(DESKTOP_WSL_PREPARE_PERMISSION)
        .permission(DESKTOP_WSL_CONNECT_PERMISSION)
        .permission(DESKTOP_REMOTE_REQUEST_PERMISSION)
        .permission(DESKTOP_REMOTE_SUBSCRIBE_PERMISSION)
        .permission(DESKTOP_REMOTE_UNSUBSCRIBE_PERMISSION)
}

fn handle_navigation(app: &AppHandle, url: &Url) -> bool {
    let allowed = app
        .try_state::<DesktopRuntime>()
        .and_then(|runtime| {
            runtime
                .navigation_origin
                .read()
                .ok()
                .and_then(|origin| origin.clone())
        })
        .is_some_and(|origin| origin == url.origin().ascii_serialization());
    if !allowed {
        open_external_url(app, url);
    }
    allowed
}

fn open_external_url(app: &AppHandle, url: &Url) {
    if matches!(url.scheme(), "http" | "https")
        && let Err(error) = app.opener().open_url(url.as_str(), None::<&str>)
    {
        log::warn!("failed to open external URL: {error}");
    }
}

fn install_close_handler(window: &WebviewWindow) {
    #[cfg(target_os = "macos")]
    let window_handle = window.clone();
    #[cfg(not(target_os = "macos"))]
    let app = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            #[cfg(target_os = "macos")]
            {
                if let Err(error) = window_handle.hide() {
                    log::warn!("failed to hide the main window: {error}");
                }
            }
            #[cfg(not(target_os = "macos"))]
            request_exit(app.clone());
        }
    });
}

fn workspace_title(workspace: &Path) -> String {
    format!("Morrow — {}", workspace_label(workspace))
}

fn workspace_location_title(workspace: &WorkspaceLocation) -> String {
    format!("Morrow — {}", workspace_location_label(workspace))
}

fn workspace_location_label(workspace: &WorkspaceLocation) -> String {
    match workspace {
        WorkspaceLocation::Local { path } => workspace_label(path),
        WorkspaceLocation::Wsl { distro, path, .. } => {
            let name = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(path);
            format!("{name} — {distro} (WSL)")
        }
    }
}

fn workspace_project_label(workspace: &WorkspaceLocation) -> String {
    match workspace {
        WorkspaceLocation::Local { path } => workspace_label(path),
        WorkspaceLocation::Wsl { path, .. } => path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(path)
            .to_string(),
    }
}

fn workspace_label(workspace: &Path) -> String {
    workspace
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.display().to_string())
}

fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
fn replace_menu(app: &AppHandle, state: &DesktopState) -> Result<(), DesktopError> {
    app.set_menu(build_menu(app, state)?)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_menu(app: &AppHandle, state: &DesktopState) -> tauri::Result<Menu<tauri::Wry>> {
    let open_folder = MenuItem::with_id(
        app,
        OPEN_FOLDER_MENU_ID,
        "Open Folder…",
        true,
        Some("CmdOrCtrl+O"),
    )?;
    let open_recent = Submenu::with_id(app, "file.open-recent", "Open Recent", true)?;
    if state.recent_workspaces().is_empty() {
        open_recent.append(&MenuItem::with_id(
            app,
            "file.open-recent.empty",
            "No Recent Folders",
            false,
            None::<&str>,
        )?)?;
    } else {
        for (index, workspace) in state.recent_workspaces().iter().enumerate() {
            open_recent.append(&MenuItem::with_id(
                app,
                format!("{OPEN_RECENT_PREFIX}{index}"),
                format!(
                    "{} — {}",
                    workspace_location_label(workspace),
                    workspace.target_label()
                ),
                true,
                None::<&str>,
            )?)?;
        }
    }
    let file_separator = PredefinedMenuItem::separator(app)?;
    let close_window = PredefinedMenuItem::close_window(app, None)?;
    let quit = PredefinedMenuItem::quit(app, None)?;
    let file_menu = Submenu::with_id_and_items(
        app,
        "file",
        "File",
        true,
        &[&open_folder, &open_recent, &file_separator, &close_window],
    )?;
    let edit_menu = Submenu::with_id_and_items(
        app,
        "edit",
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let window_menu = Submenu::with_id_and_items(
        app,
        "window",
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    let about_metadata = AboutMetadata {
        name: Some("Morrow".to_string()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        comments: Some("A local coding agent for your workspace.".to_string()),
        website: Some("https://github.com/catDforD/morrow".to_string()),
        website_label: Some("Morrow on GitHub".to_string()),
        ..Default::default()
    };
    let download_release = MenuItem::with_id(
        app,
        DOWNLOAD_RELEASE_MENU_ID,
        "Download Latest Version",
        true,
        None::<&str>,
    )?;
    let open_logs = MenuItem::with_id(app, OPEN_LOGS_MENU_ID, "Open Logs", true, None::<&str>)?;
    let help_menu = Submenu::with_id_and_items(
        app,
        "help",
        "Help",
        true,
        &[
            &download_release,
            &open_logs,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::about(app, None, Some(about_metadata.clone()))?,
        ],
    )?;

    let app_menu = Submenu::with_id_and_items(
        app,
        "app",
        "Morrow",
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(about_metadata))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    Menu::with_items(
        app,
        &[&app_menu, &file_menu, &edit_menu, &window_menu, &help_menu],
    )
}

#[cfg(target_os = "macos")]
fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref();
    match id {
        OPEN_FOLDER_MENU_ID => request_workspace_picker(app, false),
        DOWNLOAD_RELEASE_MENU_ID => {
            if let Err(error) = app.opener().open_url(RELEASES_URL, None::<&str>) {
                show_error(
                    app,
                    "Morrow could not open GitHub Releases",
                    &error.to_string(),
                );
            }
        }
        OPEN_LOGS_MENU_ID => open_logs_directory(app),
        _ => {
            let Some(index) = id
                .strip_prefix(OPEN_RECENT_PREFIX)
                .and_then(|index| index.parse::<usize>().ok())
            else {
                return;
            };
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let workspace = {
                    let runtime = handle.state::<DesktopRuntime>();
                    runtime
                        .inner
                        .lock()
                        .await
                        .state
                        .recent_workspaces()
                        .get(index)
                        .cloned()
                };
                if let Some(workspace) = workspace {
                    spawn_location_switch(handle, workspace, false);
                }
            });
        }
    }
}

fn open_logs_directory(app: &AppHandle) {
    match app.path().app_log_dir() {
        Ok(path) => {
            if let Err(error) = std::fs::create_dir_all(&path) {
                show_error(app, "Morrow could not open logs", &error.to_string());
                return;
            }
            if let Err(error) = app.opener().open_path(path.to_string_lossy(), None::<&str>) {
                show_error(app, "Morrow could not open logs", &error.to_string());
            }
        }
        Err(error) => show_error(app, "Morrow could not locate logs", &error.to_string()),
    }
}

fn handle_run_event(app: &AppHandle, event: RunEvent) {
    match event {
        RunEvent::ExitRequested {
            code: None, api, ..
        } => {
            api.prevent_exit();
            request_exit(app.clone());
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => focus_main_window(app),
        _ => {}
    }
}

fn request_exit(app: AppHandle) {
    let Some(runtime) = app.try_state::<DesktopRuntime>() else {
        app.exit(0);
        return;
    };
    if runtime
        .exit_requested
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let runtime = app.state::<DesktopRuntime>();
        let _operation = runtime.operation.lock().await;
        let (server, wsl) = {
            let mut inner = runtime.inner.lock().await;
            (inner.server.take(), inner.wsl.take())
        };
        if let Some(server) = server {
            match stop_server_with_confirmation(&app, server, "quit Morrow").await {
                Ok(StopServerOutcome::Stopped) => {}
                Ok(StopServerOutcome::Cancelled(server)) => {
                    runtime.inner.lock().await.server = Some(server);
                    runtime.exit_requested.store(false, Ordering::Release);
                    return;
                }
                Err(error) => {
                    log::error!(
                        "failed to shut down the local server cleanly ({})",
                        error.log_category()
                    );
                }
            }
        }
        if let Some(connection) = wsl {
            match stop_wsl_with_confirmation(&app, connection, "quit Morrow").await {
                Ok(StopWslOutcome::Stopped) => {}
                Ok(StopWslOutcome::Cancelled(connection)) => {
                    runtime.inner.lock().await.wsl = Some(*connection);
                    runtime.exit_requested.store(false, Ordering::Release);
                    return;
                }
                Err(error) => {
                    log::error!("failed to shut down WSL runtime cleanly: {error}");
                }
            }
        }
        app.exit(0);
    });
}

fn show_error(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message.to_string())
        .title(title.to_string())
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::Ok)
        .show(|_| {});
}

fn show_error_and_exit(app: &AppHandle, title: &str, message: &str) {
    let handle = app.clone();
    app.dialog()
        .message(message.to_string())
        .title(title.to_string())
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::Ok)
        .show(move |_| handle.exit(1));
}

fn parse_url(url: &str) -> Result<Url, DesktopError> {
    url.parse::<Url>()
        .map_err(|error| DesktopError::InvalidUrl(error.to_string()))
}

#[derive(Debug, Error)]
enum DesktopError {
    #[error("home directory could not be determined")]
    HomeDirectoryNotFound,
    #[error("failed to generate a desktop session token: {0}")]
    Random(String),
    #[error("failed to configure the local server: {0}")]
    BackendConfiguration(String),
    #[error("invalid desktop URL: {0}")]
    InvalidUrl(String),
    #[error("desktop navigation URL must use an HTTP loopback origin: {0}")]
    InvalidNavigationOrigin(String),
    #[error(transparent)]
    Config(#[from] agent_config::ConfigError),
    #[error(transparent)]
    State(#[from] DesktopStateError),
    #[error(transparent)]
    Server(#[from] agent_server::ServerError),
    #[error("WSL connection failed: {0}")]
    Wsl(String),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}

impl DesktopError {
    fn log_category(&self) -> &'static str {
        match self {
            Self::HomeDirectoryNotFound => "home-directory",
            Self::Random(_) => "random-token",
            Self::BackendConfiguration(_) => "backend-configuration",
            Self::InvalidUrl(_) => "desktop-url",
            Self::InvalidNavigationOrigin(_) => "navigation-origin",
            Self::Config(_) => "workspace-config",
            Self::State(_) => "desktop-state",
            Self::Server(_) => "local-server",
            Self::Wsl(_) => "wsl",
            Self::Tauri(_) => "tauri",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::ipc::RuntimeCapability;
    use tauri::utils::acl::capability::{CapabilityFile, PermissionEntry};

    #[test]
    fn desktop_action_deserializes_only_whitelisted_tagged_actions() {
        let cases = [
            (r#"{"type":"start_drag"}"#, DesktopAction::StartDrag {}),
            (r#"{"type":"minimize"}"#, DesktopAction::Minimize {}),
            (
                r#"{"type":"toggle_maximize"}"#,
                DesktopAction::ToggleMaximize {},
            ),
            (r#"{"type":"close_window"}"#, DesktopAction::CloseWindow {}),
            (r#"{"type":"quit"}"#, DesktopAction::Quit {}),
            (r#"{"type":"open_folder"}"#, DesktopAction::OpenFolder {}),
            (
                r#"{"type":"open_recent","index":3}"#,
                DesktopAction::OpenRecent { index: 3 },
            ),
            (r#"{"type":"open_logs"}"#, DesktopAction::OpenLogs {}),
            (
                r#"{"type":"download_latest"}"#,
                DesktopAction::DownloadLatest {},
            ),
        ];

        for (json, expected) in cases {
            assert_eq!(
                serde_json::from_str::<DesktopAction>(json).expect("deserialize desktop action"),
                expected
            );
        }
        assert!(serde_json::from_str::<DesktopAction>(r#"{"type":"run_command"}"#).is_err());
        assert!(
            serde_json::from_str::<DesktopAction>(r#"{"type":"minimize","path":"/tmp"}"#).is_err()
        );
        assert!(serde_json::from_str::<DesktopAction>(r#"{"type":"open_recent"}"#).is_err());
    }

    #[test]
    fn desktop_shell_state_serializes_with_the_frontend_wire_shape() {
        let value = serde_json::to_value(DesktopShellState {
            is_maximized: true,
            recent_workspaces: vec![RecentWorkspace {
                index: 0,
                label: "morrow".to_string(),
                target: "Local".to_string(),
                path: "/tmp/morrow".to_string(),
            }],
            active_workspace: Some(WorkspaceLocation::Local {
                path: PathBuf::from("/tmp/morrow"),
            }),
        })
        .expect("serialize desktop shell state");

        assert_eq!(
            value,
            serde_json::json!({
                "isMaximized": true,
                "recentWorkspaces": [{
                    "index": 0,
                    "label": "morrow",
                    "target": "Local",
                    "path": "/tmp/morrow"
                }],
                "activeWorkspace": {
                    "kind": "local",
                    "path": "/tmp/morrow"
                }
            })
        );
    }

    #[test]
    fn desktop_keeps_subagent_settings_on_the_windows_host() {
        for path in [
            "/api/subagent-settings",
            "/api/subagent-settings/reset",
            "/api/subagents",
            "/api/subagents/builtin-01",
        ] {
            assert!(is_windows_owned_settings_path(path), "{path}");
        }
        assert!(!is_windows_owned_settings_path("/api/sessions/default"));
    }

    #[test]
    fn desktop_status_uses_the_windows_subagent_store_path() {
        let remote = RemoteResponse::Http(agent_protocol::RemoteHttpResponse {
            status: 200,
            body: Some(serde_json::json!({
                "workspace_root": "/home/morrow/project",
                "subagent_store_path": "/home/morrow/.morrow/subagents.json"
            })),
        });
        let local = agent_server::EmbeddedHttpResponse {
            status: 200,
            body: Some(serde_json::json!({
                "model_ready": true,
                "model_store_path": "C:/Users/morrow/.morrow/web-models.json",
                "mcp_store_path": "C:/Users/morrow/.morrow/web-mcp.json",
                "command_store_path": "C:/Users/morrow/.morrow/commands",
                "subagent_store_path": "C:/Users/morrow/.morrow/subagents.json"
            })),
        };

        let RemoteResponse::Http(merged) =
            merge_remote_status(remote, local).expect("merge remote status")
        else {
            panic!("status response must remain HTTP");
        };
        let body = merged.body.expect("merged status body");
        assert_eq!(body["workspace_root"], "/home/morrow/project");
        assert_eq!(
            body["subagent_store_path"],
            "C:/Users/morrow/.morrow/subagents.json"
        );
    }

    #[test]
    fn default_workspace_is_kept_inside_the_private_morrow_home() {
        assert_eq!(
            default_workspace_path(Path::new("/home/morrow-user")),
            PathBuf::from("/home/morrow-user/.morrow/workspaces/default")
        );
    }

    #[test]
    fn desktop_app_url_is_stable_for_packaged_windows_and_development() {
        assert_eq!(
            desktop_app_url_for(None, true).expect("Windows app URL"),
            Url::parse("http://tauri.localhost").expect("valid URL")
        );
        assert_eq!(
            desktop_app_url_for(Some("http://127.0.0.1:5173"), true).expect("development app URL"),
            Url::parse("http://127.0.0.1:5173?desktop_connect=1").expect("valid URL")
        );
    }

    #[test]
    fn app_shell_navigation_skips_startup_reload_and_refreshes_explicit_switches() {
        let app_url = Url::parse("http://tauri.localhost").expect("valid app URL");
        let local_url = Url::parse("http://127.0.0.1:43123").expect("valid local URL");

        assert_eq!(
            app_shell_navigation(Some(&app_url), &app_url, false),
            AppShellNavigation::None
        );
        assert_eq!(
            app_shell_navigation(Some(&app_url), &app_url, true),
            AppShellNavigation::Reload
        );
        assert_eq!(
            app_shell_navigation(Some(&local_url), &app_url, false),
            AppShellNavigation::Navigate
        );
    }

    #[test]
    fn remote_project_label_keeps_the_target_out_of_the_project_name() {
        let workspace = WorkspaceLocation::Wsl {
            distro: "Ubuntu".to_string(),
            user: "morrow".to_string(),
            path: "/home/morrow/code/project".to_string(),
        };

        assert_eq!(workspace_project_label(&workspace), "project");
        assert_eq!(
            workspace_location_label(&workspace),
            "project — Ubuntu (WSL)"
        );
    }

    #[test]
    fn remote_capability_is_exact_remote_only_and_has_all_app_permissions() {
        let capability =
            desktop_remote_capability("desktop-shell-origin-0", "http://127.0.0.1:43123").build();
        let CapabilityFile::Capability(capability) = capability else {
            panic!("expected a single capability");
        };

        assert_eq!(capability.identifier, "desktop-shell-origin-0");
        assert!(!capability.local);
        assert_eq!(capability.windows, [MAIN_WINDOW_LABEL]);
        assert!(capability.webviews.is_empty());
        assert_eq!(
            capability.remote.expect("remote URL restriction").urls,
            ["http://127.0.0.1:43123/*"]
        );
        let permissions = capability
            .permissions
            .iter()
            .map(|permission| match permission {
                PermissionEntry::PermissionRef(identifier) => identifier.as_ref(),
                PermissionEntry::ExtendedPermission { .. } => {
                    panic!("desktop shell permissions must not be scoped plugin permissions")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            permissions,
            [
                DESKTOP_SHELL_STATE_PERMISSION,
                DESKTOP_ACTION_PERMISSION,
                DESKTOP_WSL_DISTRIBUTIONS_PERMISSION,
                DESKTOP_WSL_PROBE_PERMISSION,
                DESKTOP_WSL_PREPARE_PERMISSION,
                DESKTOP_WSL_CONNECT_PERMISSION,
                DESKTOP_REMOTE_REQUEST_PERMISSION,
                DESKTOP_REMOTE_SUBSCRIBE_PERMISSION,
                DESKTOP_REMOTE_UNSUBSCRIBE_PERMISSION,
            ]
        );
        assert!(!permissions.contains(&"core:default"));
    }

    #[test]
    fn navigation_capability_accepts_only_http_loopback_origins() {
        let loopback = parse_url("http://127.0.0.1:43123/workspace").expect("loopback URL");
        assert_eq!(
            loopback_http_origin(&loopback).expect("valid loopback origin"),
            "http://127.0.0.1:43123"
        );

        for url in [
            "https://127.0.0.1:43123/",
            "http://example.com:43123/",
            "file:///tmp/index.html",
        ] {
            let url = parse_url(url).expect("test URL");
            assert!(loopback_http_origin(&url).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_initialization_script_enables_the_desktop_shell() {
        let script = desktop_platform_initialization_script();
        assert!(script.contains(r#"platform: "linux""#));
        assert!(script.contains("Object.freeze"));
        assert!(script.contains("configurable: false"));
    }

    #[test]
    fn desktop_commands_reject_other_windows_and_invalid_recent_indexes() {
        assert!(ensure_main_window_label(MAIN_WINDOW_LABEL).is_ok());
        assert!(ensure_main_window_label("settings").is_err());
        assert!(recent_workspace_at(&DesktopState::default(), 0).is_err());
    }

    #[test]
    fn pending_wsl_requests_cannot_bypass_windows_settings() {
        for request in [
            RemoteRequest::Ping,
            RemoteRequest::Activity,
            RemoteRequest::Environment,
            RemoteRequest::ListDirectory {
                path: Some("/home/morrow".to_string()),
                show_hidden: false,
            },
        ] {
            assert!(request_allowed_while_wsl_pending(&request));
        }

        for request in [
            RemoteRequest::Http {
                method: "GET".to_string(),
                path: "/api/model-settings".to_string(),
                body: None,
            },
            RemoteRequest::SubscribeSession {
                session: "default".to_string(),
            },
            RemoteRequest::SessionMessage {
                session: "default".to_string(),
                message: serde_json::json!({
                    "type": "start_turn",
                    "data": { "prompt": "hello" }
                }),
            },
        ] {
            assert!(!request_allowed_while_wsl_pending(&request));
        }
    }

    #[tokio::test]
    async fn remote_settings_scope_supports_session_model_selection_routes() {
        let root = std::env::temp_dir().join(format!(
            "morrow-desktop-remote-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create test home");
        let location = WorkspaceLocation::Wsl {
            distro: "Ubuntu".to_string(),
            user: "morrow".to_string(),
            path: "/home/morrow/code/project".to_string(),
        };
        let server = prepare_remote_settings(
            &root,
            &location,
            RemoteWorkspaceConfiguration {
                fallback_model: None,
                fallback_mcp_servers: Vec::new(),
            },
        )
        .expect("remote settings server");

        let response = server
            .request("GET", "/api/sessions/default/model-selection", None)
            .await
            .expect("model selection response");

        assert_eq!(response.status, 200);
        assert_eq!(
            response.body.expect("response body")["selection"],
            serde_json::Value::Null
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
