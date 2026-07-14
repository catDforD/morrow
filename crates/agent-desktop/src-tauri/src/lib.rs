mod state;

use agent_config::load_server_config_for_workspace;
use agent_server::{
    RunningServer, ServerAccessPolicy, ServerOptions, ShutdownPolicy,
    server_options_from_loaded_config, spawn_local,
};
use serde::{Deserialize, Serialize};
use state::{DesktopState, DesktopStateError};
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
use std::collections::HashSet;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Manager, RunEvent, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind};
use tauri_plugin_opener::OpenerExt;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};

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

struct DesktopRuntime {
    home: PathBuf,
    state_path: PathBuf,
    bootstrap_token: String,
    inner: Mutex<DesktopRuntimeInner>,
    operation: Mutex<()>,
    navigation_origin: RwLock<Option<String>>,
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    authorized_origins: RwLock<HashSet<String>>,
    exit_requested: AtomicBool,
}

struct DesktopRuntimeInner {
    state: DesktopState,
    workspace: Option<PathBuf>,
    server: Option<RunningServer>,
}

struct StartedServer {
    server: RunningServer,
    navigation_url: Url,
}

enum StopServerOutcome {
    Stopped,
    Cancelled(RunningServer),
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecentWorkspace {
    index: usize,
    label: String,
}

#[tauri::command]
async fn desktop_shell_state(
    window: WebviewWindow,
    runtime: State<'_, DesktopRuntime>,
) -> Result<DesktopShellState, String> {
    ensure_main_window(&window)?;
    let is_maximized = window.is_maximized().map_err(|error| error.to_string())?;
    let recent_workspaces = runtime
        .inner
        .lock()
        .await
        .state
        .recent_workspaces()
        .iter()
        .enumerate()
        .map(|(index, workspace)| RecentWorkspace {
            index,
            label: workspace_label(workspace),
        })
        .collect();
    Ok(DesktopShellState {
        is_maximized,
        recent_workspaces,
    })
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
            spawn_workspace_switch(app, workspace, false);
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

fn recent_workspace_at(state: &DesktopState, index: usize) -> Result<PathBuf, String> {
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
            desktop_action
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
    let initial_workspace = state.last_workspace().map(Path::to_path_buf);
    let bootstrap_token = generate_bootstrap_token()?;

    app.manage(DesktopRuntime {
        home,
        state_path,
        bootstrap_token,
        inner: Mutex::new(DesktopRuntimeInner {
            state: state.clone(),
            workspace: None,
            server: None,
        }),
        operation: Mutex::new(()),
        navigation_origin: RwLock::new(None),
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
        authorized_origins: RwLock::new(HashSet::new()),
        exit_requested: AtomicBool::new(false),
    });
    #[cfg(target_os = "macos")]
    replace_menu(app.handle(), &state)?;
    log::info!("Morrow desktop started");

    if let Some(workspace) = initial_workspace {
        spawn_workspace_switch(app.handle().clone(), workspace, true);
    } else {
        request_workspace_picker(app.handle(), true);
    }
    Ok(())
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
    {
        let inner = runtime.inner.lock().await;
        if inner.workspace.as_ref() == Some(&workspace) {
            focus_main_window(app);
            return Ok(());
        }
    }
    let options = prepare_server_options(&runtime.home, &workspace)?;

    let (old_server, old_workspace) = {
        let mut inner = runtime.inner.lock().await;
        (inner.server.take(), inner.workspace.clone())
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

    let started = match launch_server(options, &runtime.bootstrap_token).await {
        Ok(started) => started,
        Err(error) => {
            if let Some(old_workspace) = old_workspace {
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
            return Err(error);
        }
    };

    authorize_navigation_origin(app, &runtime, &started.navigation_url)?;
    let mut next_state = {
        let inner = runtime.inner.lock().await;
        inner.state.clone()
    };
    next_state.record_workspace(&workspace)?;
    let navigation_url = started.navigation_url.clone();
    {
        let mut inner = runtime.inner.lock().await;
        inner.state = next_state.clone();
        inner.workspace = Some(workspace.clone());
        inner.server = Some(started.server);
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
    #[cfg(any(target_os = "windows", target_os = "linux"))]
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
                workspace.display().to_string(),
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
                    spawn_workspace_switch(handle, workspace, false);
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
        let server = runtime.inner.lock().await.server.take();
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
            }],
        })
        .expect("serialize desktop shell state");

        assert_eq!(
            value,
            serde_json::json!({
                "isMaximized": true,
                "recentWorkspaces": [{
                    "index": 0,
                    "label": "morrow"
                }]
            })
        );
    }

    #[test]
    fn remote_capability_is_exact_remote_only_and_has_two_app_permissions() {
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
            [DESKTOP_SHELL_STATE_PERMISSION, DESKTOP_ACTION_PERMISSION]
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
}
