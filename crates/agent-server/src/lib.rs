//! HTTP, WebSocket, desktop-security, and embedded JSON adapters for [`agent_app`].

pub use agent_app::{
    FallbackModel, RunningTurnSnapshot, SubagentProfileResponse, SubagentProfileWriteRequest,
    SubagentRegistryError, SubagentRoleSettingsResponse, SubagentRoleWriteRequest,
    SubagentSettingsResponse, SubagentTranscriptSnapshot, WorkspaceEvent as ServerMessage,
    discover_remote_models, load_subagent_identities,
};

use agent_app::{
    CommandResponse, CommandSettingsResponse, CommandWriteRequest, DiscoverModelsRequest,
    DiscoverModelsResponse, McpServerResponse, McpServerTestRequest, McpServerWriteRequest,
    McpSettingsResponse, ModelProviderResponse, ModelRegistryError, ModelSettingsResponse,
    ProviderWriteRequest, ResolveCommandRequest, ResolveCommandResponse, SessionArchive,
    SessionCommand, SessionEntry, SessionModelSelectionResponse, SessionSubscription,
    SubscriptionError, WorkspaceApp, WorkspaceError, WorkspaceErrorKind, WorkspaceOptions,
    WorkspaceStatus,
};
use agent_config::{ContextConfig, LoadedServerConfig, McpServerConfig};
use agent_model::ModelError;
use agent_protocol::{
    ModelSelection, PermissionMode, PermissionProfile, RemoteMcpServerSpec,
    RemoteModelConnectionSpec, RemoteSubagentMessageSpec, RemoteTurnSpec, SessionDocument,
    SubagentRole, WorkspaceLocation,
};
use agent_runtime::McpInspection;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower::ServiceExt;

pub const DEFAULT_WEB_PERMISSION_MODE: PermissionMode = PermissionMode::WorkspaceWrite;

#[derive(Clone)]
pub struct ServerOptions {
    pub host: IpAddr,
    pub port: u16,
    pub fallback_model: Option<FallbackModel>,
    pub model_store_path: PathBuf,
    pub mcp_store_path: PathBuf,
    pub command_store_path: PathBuf,
    pub subagent_store_path: PathBuf,
    pub system_prompt: String,
    pub context_config: ContextConfig,
    pub workspace_root: PathBuf,
    pub workspace_location: WorkspaceLocation,
    pub config_path: Option<PathBuf>,
    pub config_diagnostics: Vec<String>,
    /// Default for legacy clients that do not select a permission mode per turn.
    pub permissions: PermissionProfile,
    pub mcp_servers: Vec<McpServerConfig>,
    pub default_session_name: String,
}

impl ServerOptions {
    fn workspace_options(&self, persistent_settings: bool) -> WorkspaceOptions {
        WorkspaceOptions {
            fallback_model: self.fallback_model.clone(),
            model_store_path: self.model_store_path.clone(),
            mcp_store_path: self.mcp_store_path.clone(),
            command_store_path: self.command_store_path.clone(),
            subagent_store_path: self.subagent_store_path.clone(),
            system_prompt: self.system_prompt.clone(),
            context_config: self.context_config,
            workspace_root: self.workspace_root.clone(),
            workspace_location: self.workspace_location.clone(),
            config_path: self.config_path.clone(),
            config_diagnostics: self.config_diagnostics.clone(),
            permissions: self.permissions,
            mcp_servers: self.mcp_servers.clone(),
            default_session_name: self.default_session_name.clone(),
            persistent_settings,
        }
    }
}

pub fn server_options_from_loaded_config(
    host: IpAddr,
    port: u16,
    workspace_root: PathBuf,
    home: &std::path::Path,
    loaded: LoadedServerConfig,
    default_session_name: String,
) -> Result<ServerOptions, ModelError> {
    let options = agent_app::workspace_options_from_loaded_config(
        workspace_root,
        home,
        loaded,
        default_session_name,
        PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
    )?;
    Ok(ServerOptions {
        host,
        port,
        fallback_model: options.fallback_model,
        model_store_path: options.model_store_path,
        mcp_store_path: options.mcp_store_path,
        command_store_path: options.command_store_path,
        subagent_store_path: options.subagent_store_path,
        system_prompt: options.system_prompt,
        context_config: options.context_config,
        workspace_root: options.workspace_root,
        workspace_location: options.workspace_location,
        config_path: options.config_path,
        config_diagnostics: options.config_diagnostics,
        permissions: options.permissions,
        mcp_servers: options.mcp_servers,
        default_session_name: options.default_session_name,
    })
}

#[derive(Clone, Default)]
pub enum ServerAccessPolicy {
    #[default]
    Browser,
    Desktop {
        token: Arc<str>,
    },
    Embedded,
}

impl ServerAccessPolicy {
    pub fn desktop(token: impl Into<String>) -> Self {
        Self::Desktop {
            token: Arc::from(token.into()),
        }
    }
}

impl std::fmt::Debug for ServerAccessPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Browser => formatter.write_str("Browser"),
            Self::Desktop { .. } => formatter
                .debug_struct("Desktop")
                .field("token", &"<redacted>")
                .finish(),
            Self::Embedded => formatter.write_str("Embedded"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerActivity {
    pub running_turns: usize,
    pub pending_approvals: usize,
}

impl ServerActivity {
    pub fn is_idle(self) -> bool {
        self.running_turns == 0
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ShutdownPolicy {
    RequireIdle,
    CancelRunning { timeout: Duration },
}

pub struct RunningServer {
    addr: SocketAddr,
    state: WorkspaceService,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServerError>>>,
}

#[derive(Clone)]
pub struct EmbeddedServer {
    router: Router,
    service: WorkspaceService,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedHttpResponse {
    pub status: u16,
    pub body: Option<serde_json::Value>,
}

pub struct EmbeddedSessionSubscription {
    pub snapshot: serde_json::Value,
    subscription: SessionSubscription,
}

impl EmbeddedSessionSubscription {
    pub async fn recv(&mut self) -> Result<serde_json::Value, String> {
        loop {
            match self.subscription.recv().await {
                Ok(message) => {
                    return serde_json::to_value(message).map_err(|error| error.to_string());
                }
                Err(SubscriptionError::Lagged(_)) => continue,
                Err(SubscriptionError::Closed) => {
                    return Err("session event stream closed".to_string());
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct WorkspaceService {
    app: WorkspaceApp,
    transport: Arc<TransportState>,
}

struct TransportState {
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
}

type AppState = WorkspaceService;

impl EmbeddedServer {
    pub fn new(options: ServerOptions) -> Result<Self, ModelRegistryError> {
        let (router, service) = build_router(options, ServerAccessPolicy::Embedded)?;
        Ok(Self { router, service })
    }

    pub fn new_workspace(options: ServerOptions) -> Result<Self, ModelRegistryError> {
        let (router, service) = build_workspace_router(options, ServerAccessPolicy::Embedded)?;
        Ok(Self { router, service })
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<EmbeddedHttpResponse, String> {
        if !path.starts_with('/') {
            return Err("embedded request path must start with '/'".to_string());
        }
        let method = Method::from_bytes(method.as_bytes()).map_err(|error| error.to_string())?;
        let mut builder = Request::builder().method(method).uri(path);
        let request_body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&value).map_err(|error| error.to_string())?)
            }
            None => Body::empty(),
        };
        let request = builder
            .body(request_body)
            .map_err(|error| error.to_string())?;
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(|error| error.to_string())?)
        };
        Ok(EmbeddedHttpResponse { status, body })
    }

    pub async fn subscribe_session(
        &self,
        session_name: &str,
    ) -> Result<EmbeddedSessionSubscription, String> {
        self.service.subscribe_session(session_name).await
    }

    pub async fn send_session_message(
        &self,
        session_name: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        self.service.send_session_message(session_name, value).await
    }

    pub async fn prepare_remote_turn(
        &self,
        session_name: &str,
        value: serde_json::Value,
    ) -> Result<RemoteTurnSpec, String> {
        let message = serde_json::from_value::<ClientMessage>(value)
            .map_err(|error| format!("invalid session message: {error}"))?;
        let ClientMessage::StartTurn {
            request_id,
            prompt,
            prompt_resolved,
            permission_mode,
            model_selection,
        } = message
        else {
            return Err("only start_turn can be prepared for a remote workspace".to_string());
        };
        self.service
            .app
            .prepare_remote_turn(
                session_name,
                request_id,
                prompt,
                prompt_resolved,
                permission_mode,
                model_selection,
            )
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn prepare_remote_subagent_message(
        &self,
        session_name: &str,
        message: serde_json::Value,
    ) -> Result<RemoteSubagentMessageSpec, String> {
        self.service
            .app
            .prepare_remote_subagent_message(session_name, message)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn start_remote_turn(&self, turn: RemoteTurnSpec) -> Result<(), String> {
        self.service
            .app
            .start_remote_turn(turn)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn send_remote_subagent_message(
        &self,
        command: RemoteSubagentMessageSpec,
    ) -> Result<(), String> {
        self.service
            .app
            .send_remote_subagent_message(command)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn prepare_remote_model_discovery(
        &self,
        value: serde_json::Value,
    ) -> Result<RemoteModelConnectionSpec, String> {
        let request = serde_json::from_value::<DiscoverModelsRequest>(value)
            .map_err(|error| format!("invalid model discovery request: {error}"))?;
        self.service
            .app
            .prepare_remote_model_discovery(request)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn prepare_remote_mcp_test(
        &self,
        value: serde_json::Value,
    ) -> Result<RemoteMcpServerSpec, String> {
        let request = serde_json::from_value::<McpServerTestRequest>(value)
            .map_err(|error| format!("invalid MCP test request: {error}"))?;
        self.service
            .app
            .prepare_remote_mcp_test(request)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn inspect_remote_mcp(&self, server: RemoteMcpServerSpec) -> McpInspection {
        self.service.app.inspect_remote_mcp(server).await
    }

    pub async fn activity(&self) -> ServerActivity {
        self.service.activity().await
    }

    pub async fn shutdown(&self, cancel_running: bool) {
        self.service.shutdown(cancel_running).await;
    }
}

impl WorkspaceService {
    pub async fn subscribe_session(
        &self,
        session_name: &str,
    ) -> Result<EmbeddedSessionSubscription, String> {
        let subscription = self
            .app
            .subscribe_session(session_name)
            .await
            .map_err(|error| error.to_string())?;
        let snapshot =
            serde_json::to_value(&subscription.snapshot).map_err(|error| error.to_string())?;
        Ok(EmbeddedSessionSubscription {
            snapshot,
            subscription,
        })
    }

    pub async fn send_session_message(
        &self,
        session_name: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let message = serde_json::from_value::<ClientMessage>(value)
            .map_err(|error| format!("invalid session message: {error}"))?;
        self.app
            .send_session_command(session_name, app_command(message, &self.app))
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn activity(&self) -> ServerActivity {
        let activity = self.app.activity().await;
        ServerActivity {
            running_turns: activity.running_turns,
            pending_approvals: activity.pending_approvals,
        }
    }

    pub async fn shutdown(&self, cancel_running: bool) {
        self.app.shutdown(cancel_running).await;
    }
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn activity(&self) -> ServerActivity {
        self.state.activity().await
    }

    pub async fn shutdown(&mut self, policy: ShutdownPolicy) -> Result<(), ServerError> {
        self.state.app.begin_shutdown();
        let activity = self.state.activity().await;
        match policy {
            ShutdownPolicy::RequireIdle if !activity.is_idle() => {
                self.state.app.resume_after_shutdown_rejection();
                return Err(ServerError::RunningTurns(activity.running_turns));
            }
            ShutdownPolicy::RequireIdle => self.state.app.shutdown(false).await,
            ShutdownPolicy::CancelRunning { timeout } => {
                self.state.app.shutdown_with_timeout(true, timeout).await;
            }
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(result) => return result.map_err(ServerError::Task)?,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        Ok(())
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    ModelSettings(#[from] ModelRegistryError),
    #[error("failed to bind server at {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("server failed: {0}")]
    Serve(#[source] std::io::Error),
    #[error("server task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
    #[error("server has {0} running turn(s)")]
    RunningTurns(usize),
}

pub async fn serve(mut options: ServerOptions) -> Result<(), ServerError> {
    let addr = SocketAddr::new(options.host, options.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    let bound_addr = listener
        .local_addr()
        .map_err(|source| ServerError::Bind { addr, source })?;
    options.host = bound_addr.ip();
    options.port = bound_addr.port();
    axum::serve(listener, router(options)?)
        .await
        .map_err(ServerError::Serve)
}

pub fn router(options: ServerOptions) -> Result<Router, ModelRegistryError> {
    build_router(options, ServerAccessPolicy::Browser).map(|(router, _)| router)
}

pub async fn spawn_local(
    mut options: ServerOptions,
    access_policy: ServerAccessPolicy,
) -> Result<RunningServer, ServerError> {
    let addr = SocketAddr::new(options.host, options.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    let bound_addr = listener
        .local_addr()
        .map_err(|source| ServerError::Bind { addr, source })?;
    options.host = bound_addr.ip();
    options.port = bound_addr.port();
    let (router, state) = build_router(options, access_policy)?;
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = receiver.await;
            })
            .await
            .map_err(ServerError::Serve)
    });
    Ok(RunningServer {
        addr: bound_addr,
        state,
        shutdown: Some(shutdown),
        task: Some(task),
    })
}

fn build_router(
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
) -> Result<(Router, AppState), ModelRegistryError> {
    build_router_with_settings(options, access_policy, true)
}

fn build_workspace_router(
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
) -> Result<(Router, AppState), ModelRegistryError> {
    build_router_with_settings(options, access_policy, false)
}

fn build_router_with_settings(
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
    persistent_settings: bool,
) -> Result<(Router, AppState), ModelRegistryError> {
    let app = WorkspaceApp::new_with_model_registry_error(
        options.workspace_options(persistent_settings),
    )?;
    let state = WorkspaceService {
        app,
        transport: Arc::new(TransportState {
            options,
            access_policy,
        }),
    };
    let mut router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/assets/{*path}", get(asset))
        .route("/api/status", get(status))
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/{name}",
            get(get_session).post(create_session),
        )
        .route("/api/sessions/{name}/reset", post(reset_session))
        .route("/api/sessions/{name}/archive", post(archive_session))
        .route("/api/sessions/{name}/restore", post(restore_session))
        .route("/api/sessions/{name}/ws", get(session_ws));
    if persistent_settings {
        router = router
            .route("/api/model-settings", get(model_settings))
            .route("/api/model-providers", post(create_model_provider))
            .route(
                "/api/model-providers/{provider_id}",
                put(update_model_provider).delete(delete_model_provider),
            )
            .route(
                "/api/model-providers/discover",
                post(discover_model_provider),
            )
            .route("/api/model-default", put(set_default_model))
            .route("/api/mcp-settings", get(mcp_settings))
            .route("/api/mcp-servers", post(create_mcp_server))
            .route("/api/mcp-servers/import", post(import_mcp_servers))
            .route("/api/mcp-servers/test", post(test_mcp_server))
            .route(
                "/api/mcp-servers/{name}",
                put(update_mcp_server).delete(delete_mcp_server),
            )
            .route("/api/commands", get(command_settings).post(create_command))
            .route("/api/commands/resolve", post(resolve_command))
            .route(
                "/api/commands/{name}",
                put(update_command).delete(delete_command),
            )
            .route("/api/subagent-settings", get(subagent_settings))
            .route(
                "/api/subagent-settings/roles/{role}",
                put(update_subagent_role),
            )
            .route(
                "/api/subagent-settings/roles/reset",
                post(reset_subagent_roles),
            )
            .route("/api/subagents", post(create_subagent))
            .route(
                "/api/subagents/{id}",
                put(update_subagent).delete(delete_subagent),
            )
            .route("/api/subagent-settings/reset", post(reset_subagents))
            .route(
                "/api/sessions/{name}/model-selection",
                get(get_session_model_selection).put(set_session_model_selection),
            );
    }
    let router = router
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_middleware,
        ));
    Ok((router, state))
}

async fn access_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if matches!(state.transport.access_policy, ServerAccessPolicy::Embedded) {
        return next.run(request).await;
    }
    let response = match &state.transport.access_policy {
        ServerAccessPolicy::Browser => next.run(request).await,
        ServerAccessPolicy::Embedded => next.run(request).await,
        ServerAccessPolicy::Desktop { token } => {
            let expected_host = format!(
                "{}:{}",
                state.transport.options.host, state.transport.options.port
            );
            let host_matches = request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|host| host == expected_host);
            if !host_matches {
                StatusCode::UNAUTHORIZED.into_response()
            } else if is_bootstrap_request(&request, token) {
                let mut response = StatusCode::SEE_OTHER.into_response();
                response
                    .headers_mut()
                    .insert(header::LOCATION, HeaderValue::from_static("/"));
                let cookie =
                    format!("morrow_desktop_session={token}; HttpOnly; SameSite=Strict; Path=/");
                if let Ok(value) = HeaderValue::from_str(&cookie) {
                    response.headers_mut().insert(header::SET_COOKIE, value);
                }
                response
            } else if !has_desktop_cookie(&request, token)
                || !origin_is_allowed(&request, &expected_host)
            {
                StatusCode::UNAUTHORIZED.into_response()
            } else {
                next.run(request).await
            }
        }
    };
    with_security_headers(response)
}

fn is_bootstrap_request(request: &Request<Body>, token: &str) -> bool {
    request.method() == Method::GET
        && request.uri().path() == "/"
        && request
            .uri()
            .query()
            .and_then(|query| {
                query.split('&').find_map(|pair| {
                    pair.strip_prefix("desktop_bootstrap=")
                        .filter(|value| !value.contains('='))
                })
            })
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
}

fn has_desktop_cookie(request: &Request<Body>, token: &str) -> bool {
    request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookies| {
            cookies.split(';').any(|cookie| {
                cookie
                    .trim()
                    .strip_prefix("morrow_desktop_session=")
                    .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
            })
        })
}

fn origin_is_allowed(request: &Request<Body>, expected_host: &str) -> bool {
    let requires_origin = request.uri().path().ends_with("/ws")
        || !matches!(*request.method(), Method::GET | Method::HEAD);
    if !requires_origin {
        return true;
    }
    let expected_origin = format!("http://{expected_host}");
    request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == expected_origin)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn with_security_headers(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self' ws: wss:; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<WorkspaceError> for ApiError {
    fn from(error: WorkspaceError) -> Self {
        let status = match error.kind() {
            WorkspaceErrorKind::Validation => StatusCode::BAD_REQUEST,
            WorkspaceErrorKind::Conflict => StatusCode::CONFLICT,
            WorkspaceErrorKind::NotFound => StatusCode::NOT_FOUND,
            WorkspaceErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
            })),
        )
            .into_response()
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn app_js() -> Response {
    asset_response("app.js")
}

async fn style_css() -> Response {
    asset_response("style.css")
}

async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&path)
}

fn asset_response(path: &str) -> Response {
    match path {
        "app.js" => (
            [(
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            )],
            include_str!("../assets/app.js"),
        )
            .into_response(),
        "style.css" => (
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            include_str!("../assets/style.css"),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn status(State(state): State<AppState>) -> Json<WorkspaceStatus> {
    Json(state.app.status().await)
}

async fn model_settings(State(state): State<AppState>) -> Json<ModelSettingsResponse> {
    Json(state.app.model_settings().await)
}

async fn create_model_provider(
    State(state): State<AppState>,
    Json(request): Json<ProviderWriteRequest>,
) -> Result<Json<ModelProviderResponse>, ApiError> {
    state
        .app
        .create_model_provider(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn update_model_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Json(request): Json<ProviderWriteRequest>,
) -> Result<Json<ModelProviderResponse>, ApiError> {
    state
        .app
        .update_model_provider(&provider_id, request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn delete_model_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.app.delete_model_provider(&provider_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn discover_model_provider(
    State(state): State<AppState>,
    Json(request): Json<DiscoverModelsRequest>,
) -> Result<Json<DiscoverModelsResponse>, ApiError> {
    state
        .app
        .discover_models(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn set_default_model(
    State(state): State<AppState>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<ModelSelection>, ApiError> {
    state
        .app
        .set_default_model(selection)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn mcp_settings(State(state): State<AppState>) -> Json<McpSettingsResponse> {
    Json(state.app.mcp_settings().await)
}

async fn create_mcp_server(
    State(state): State<AppState>,
    Json(request): Json<McpServerWriteRequest>,
) -> Result<Json<McpServerResponse>, ApiError> {
    state
        .app
        .create_mcp_server(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn update_mcp_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<McpServerWriteRequest>,
) -> Result<Json<McpServerResponse>, ApiError> {
    state
        .app
        .update_mcp_server(&name, request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn delete_mcp_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.app.delete_mcp_server(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn import_mcp_servers(
    State(state): State<AppState>,
    Json(value): Json<serde_json::Value>,
) -> Result<Json<Vec<McpServerResponse>>, ApiError> {
    state
        .app
        .import_mcp_servers(value)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn test_mcp_server(
    State(state): State<AppState>,
    Json(request): Json<McpServerTestRequest>,
) -> Result<Json<McpInspection>, ApiError> {
    state
        .app
        .test_mcp_server(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn command_settings(
    State(state): State<AppState>,
) -> Result<Json<CommandSettingsResponse>, ApiError> {
    state
        .app
        .command_settings()
        .map(Json)
        .map_err(ApiError::from)
}

async fn create_command(
    State(state): State<AppState>,
    Json(request): Json<CommandWriteRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    state
        .app
        .create_command(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn update_command(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandWriteRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    state
        .app
        .update_command(&name, request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn delete_command(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.app.delete_command(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resolve_command(
    State(state): State<AppState>,
    Json(request): Json<ResolveCommandRequest>,
) -> Result<Json<ResolveCommandResponse>, ApiError> {
    state
        .app
        .resolve_command(request)
        .map(Json)
        .map_err(ApiError::from)
}

async fn subagent_settings(State(state): State<AppState>) -> Json<SubagentSettingsResponse> {
    Json(state.app.subagent_settings().await)
}

async fn update_subagent_role(
    State(state): State<AppState>,
    Path(role): Path<SubagentRole>,
    Json(request): Json<SubagentRoleWriteRequest>,
) -> Result<Json<SubagentRoleSettingsResponse>, ApiError> {
    state
        .app
        .update_subagent_role(role, request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn reset_subagent_roles(
    State(state): State<AppState>,
) -> Result<Json<Vec<SubagentRoleSettingsResponse>>, ApiError> {
    state
        .app
        .reset_subagent_roles()
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn create_subagent(
    State(state): State<AppState>,
    Json(request): Json<SubagentProfileWriteRequest>,
) -> Result<Json<SubagentProfileResponse>, ApiError> {
    state
        .app
        .create_subagent(request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn update_subagent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<SubagentProfileWriteRequest>,
) -> Result<Json<SubagentProfileResponse>, ApiError> {
    state
        .app
        .update_subagent(&id, request)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn delete_subagent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.app.delete_subagent(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_subagents(
    State(state): State<AppState>,
) -> Result<Json<SubagentSettingsResponse>, ApiError> {
    state
        .app
        .reset_subagents()
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn get_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    state
        .app
        .session_model_selection(&name)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn set_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    state
        .app
        .set_session_model_selection(&name, selection)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn list_sessions(State(state): State<AppState>) -> Result<Json<Vec<SessionEntry>>, ApiError> {
    state
        .app
        .list_sessions()
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn get_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    state.app.session(&name).map(Json).map_err(ApiError::from)
}

async fn create_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    state
        .app
        .create_session(&name)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn reset_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    state
        .app
        .reset_session(&name)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn archive_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionArchive>, ApiError> {
    state
        .app
        .archive_session(&name)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn restore_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionArchive>, ApiError> {
    state
        .app
        .restore_session(&name)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

async fn session_ws(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, name))
}

async fn handle_socket(socket: WebSocket, state: AppState, session_name: String) {
    let mut subscription = match state.app.subscribe_session(&session_name).await {
        Ok(subscription) => subscription,
        Err(error) => {
            let (mut sender, _) = socket.split();
            let message = ServerMessage::Error {
                message: error.to_string(),
            };
            let _ = send_server_message(&mut sender, &message).await;
            return;
        }
    };
    let (mut sender, mut receiver) = socket.split();
    if send_server_message(&mut sender, &subscription.snapshot)
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                if !handle_client_ws_message(message, &state, &session_name).await {
                    break;
                }
            }
            event = subscription.recv() => {
                match event {
                    Ok(message) => {
                        if send_server_message(&mut sender, &message).await.is_err() {
                            break;
                        }
                    }
                    Err(SubscriptionError::Lagged(_)) => {}
                    Err(SubscriptionError::Closed) => break,
                }
            }
        }
    }
}

async fn send_server_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

async fn handle_client_ws_message(message: Message, state: &AppState, session_name: &str) -> bool {
    let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => return false,
        _ => return true,
    };
    let parsed = serde_json::from_str::<ClientMessage>(&text);
    let Ok(message) = parsed else {
        state
            .app
            .report_session_error(session_name, "invalid websocket message")
            .await;
        return true;
    };
    let _ = state
        .app
        .send_session_command(session_name, app_command(message, &state.app))
        .await;
    true
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMessage {
    StartTurn {
        request_id: String,
        prompt: String,
        #[serde(default)]
        prompt_resolved: bool,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        model_selection: Option<ModelSelection>,
    },
    ApprovalDecision {
        request_id: String,
        approved: bool,
    },
    CancelTurn {
        turn_id: String,
    },
    SpawnSubagent {
        request_id: String,
        role: SubagentRole,
        task: String,
    },
    SendSubagent {
        request_id: String,
        instance_id: String,
        message: String,
        #[serde(default)]
        model_selection: Option<ModelSelection>,
    },
    InspectSubagent {
        instance_id: String,
    },
    CancelSubagent {
        instance_id: String,
    },
    DeleteSubagent {
        instance_id: String,
    },
}

fn app_command(message: ClientMessage, app: &WorkspaceApp) -> SessionCommand {
    match message {
        ClientMessage::StartTurn {
            request_id,
            prompt,
            prompt_resolved,
            permission_mode,
            model_selection,
        } => SessionCommand::StartTurn {
            request_id,
            prompt,
            prompt_resolved,
            permissions: permission_mode
                .map(PermissionProfile::for_mode)
                .unwrap_or_else(|| app.default_permissions()),
            model_selection,
        },
        ClientMessage::ApprovalDecision {
            request_id,
            approved,
        } => SessionCommand::ApprovalDecision {
            request_id,
            approved,
        },
        ClientMessage::CancelTurn { turn_id } => SessionCommand::CancelTurn { turn_id },
        ClientMessage::SpawnSubagent {
            request_id,
            role,
            task,
        } => SessionCommand::SpawnSubagent {
            request_id,
            role,
            task,
        },
        ClientMessage::SendSubagent {
            request_id,
            instance_id,
            message,
            model_selection,
        } => SessionCommand::SendSubagent {
            request_id,
            instance_id,
            message,
            model_selection,
        },
        ClientMessage::InspectSubagent { instance_id } => {
            SessionCommand::InspectSubagent { instance_id }
        }
        ClientMessage::CancelSubagent { instance_id } => {
            SessionCommand::CancelSubagent { instance_id }
        }
        ClientMessage::DeleteSubagent { instance_id } => {
            SessionCommand::DeleteSubagent { instance_id }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_config::ModelContextLimits;
    use agent_model::{OpenAiCompatClient, OpenAiCompatConfig};
    use agent_protocol::{ReasoningLevel, ReasoningProfile, RemoteTurnModel};
    use std::fs;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::Mutex as AsyncMutex;

    static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    static TEST_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn unique_test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "morrow-server-{name}-{}-{}-{}",
            agent_runtime::timestamp_ms(),
            std::process::id(),
            TEST_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn test_options() -> ServerOptions {
        let root = unique_test_dir("options");
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test-model".to_string(),
            api_key: "secret-test-key".to_string(),
            timeout: Duration::from_secs(1),
        })
        .expect("client");
        ServerOptions {
            host: "127.0.0.1".parse().expect("host"),
            port: 0,
            fallback_model: Some(FallbackModel {
                provider_name: "Current config".to_string(),
                model_id: "test-model".to_string(),
                model_name: "test-model".to_string(),
                client: Some(client),
                limits: ModelContextLimits {
                    context_window_tokens: 65_536,
                    reserved_output_tokens: 8_192,
                },
                reasoning_profile: ReasoningProfile::None,
            }),
            model_store_path: root.join("web-models.json"),
            mcp_store_path: root.join("web-mcp.json"),
            command_store_path: root.join("commands"),
            subagent_store_path: root.join("subagents.json"),
            system_prompt: "system".to_string(),
            context_config: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 2,
            },
            workspace_root: root.clone(),
            workspace_location: WorkspaceLocation::Local { path: root.clone() },
            config_path: Some(root.join("morrow.toml")),
            config_diagnostics: Vec::new(),
            permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
            mcp_servers: Vec::new(),
            default_session_name: "default".to_string(),
        }
    }

    #[tokio::test]
    async fn http_status_matches_direct_application_status_and_omits_api_key() {
        let server = EmbeddedServer::new(test_options()).expect("embedded server");
        let direct =
            serde_json::to_value(server.service.app.status().await).expect("direct status");
        let response = server
            .request("GET", "/api/status", None)
            .await
            .expect("HTTP status");

        assert_eq!(response.status, 200);
        assert_eq!(response.body.as_ref(), Some(&direct));
        assert_eq!(direct["permissions"]["mode"], "workspace_write");
        assert!(!direct.to_string().contains("secret-test-key"));
    }

    #[tokio::test]
    async fn embedded_subagent_settings_routes_delegate_to_application_registry() {
        let server = EmbeddedServer::new(test_options()).expect("embedded server");
        let settings = server
            .request("GET", "/api/subagent-settings", None)
            .await
            .expect("read settings");
        assert_eq!(settings.status, 200);
        assert_eq!(
            settings
                .body
                .as_ref()
                .and_then(|body| body["profiles"].as_array())
                .map(Vec::len),
            Some(22)
        );

        let created = server
            .request(
                "POST",
                "/api/subagents",
                Some(json!({"name": "测试成员", "avatar_data_url": null})),
            )
            .await
            .expect("create profile");
        assert_eq!(created.status, 200);
        let id = created.body.expect("body")["id"]
            .as_str()
            .expect("id")
            .to_string();
        assert_eq!(
            server
                .request("DELETE", &format!("/api/subagents/{id}"), None)
                .await
                .expect("delete")
                .status,
            204
        );
    }

    #[tokio::test]
    async fn session_http_crud_and_direct_listing_are_consistent() {
        let _guard = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
        let home = unique_test_dir("session-home");
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let server = EmbeddedServer::new(test_options()).expect("embedded server");

        let created = server
            .request("POST", "/api/sessions/work", None)
            .await
            .expect("create session");
        assert_eq!(created.status, 200);
        let archived = server
            .request("POST", "/api/sessions/work/archive", None)
            .await
            .expect("archive session");
        assert_eq!(archived.body.expect("archive body")["archived"], true);
        let direct = server
            .service
            .app
            .list_sessions()
            .await
            .expect("direct list");
        let listed = server
            .request("GET", "/api/sessions", None)
            .await
            .expect("HTTP list");
        assert_eq!(
            listed.body,
            Some(serde_json::to_value(&direct).expect("direct listing JSON"))
        );
        assert!(
            direct
                .iter()
                .any(|entry| entry.name == "work" && entry.archived)
        );
        assert_eq!(
            server
                .request("POST", "/api/sessions/work/restore", None)
                .await
                .expect("restore")
                .status,
            200
        );

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[tokio::test]
    async fn embedded_settings_prepare_remote_runtime_without_leaking_secrets() {
        let server = EmbeddedServer::new(test_options()).expect("embedded server");
        let provider = server
            .request(
                "POST",
                "/api/model-providers",
                Some(json!({
                    "name": "Managed",
                    "base_url": "https://models.example/v1",
                    "api_key": "managed-model-secret",
                    "enabled": true,
                    "timeout_secs": 30,
                    "models": [{
                        "id": "managed-model",
                        "name": "Managed model",
                        "context_window_tokens": 32000,
                        "reserved_output_tokens": 4000,
                        "supports_tools": true,
                        "reasoning_profile": "none"
                    }]
                })),
            )
            .await
            .expect("create provider");
        let provider_id = provider.body.expect("provider body")["id"]
            .as_str()
            .expect("provider id")
            .to_string();
        let turn = server
            .prepare_remote_turn(
                "default",
                json!({
                    "type": "start_turn",
                    "data": {
                        "request_id": "request-1",
                        "prompt": "hello",
                        "prompt_resolved": true,
                        "permission_mode": "workspace_write",
                        "model_selection": {
                            "provider_id": provider_id,
                            "model_id": "managed-model",
                            "reasoning": "off"
                        }
                    }
                }),
            )
            .await
            .expect("prepare remote turn");
        let RemoteTurnModel::Managed(model) = turn.model else {
            panic!("managed model expected");
        };
        assert_eq!(model.api_key, "managed-model-secret");
        assert!(!format!("{model:?}").contains("managed-model-secret"));
        assert_eq!(turn.subagent_identities.len(), 22);
    }

    #[tokio::test]
    async fn browser_router_serves_root_and_legacy_asset_paths() {
        let router = router(test_options()).expect("router");
        for (path, content_type) in [
            ("/app.js", "application/javascript; charset=utf-8"),
            ("/style.css", "text/css; charset=utf-8"),
            ("/assets/app.js", "application/javascript; charset=utf-8"),
            ("/assets/style.css", "text/css; charset=utf-8"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static(content_type)),
                "{path}"
            );
        }
    }

    #[test]
    fn start_turn_wire_message_maps_to_full_permission_profile() {
        let options = test_options();
        let app = WorkspaceApp::new(options.workspace_options(false)).expect("app");
        let selected = serde_json::from_value::<ClientMessage>(json!({
            "type": "start_turn",
            "data": {
                "request_id": "request-1",
                "prompt": "edit",
                "prompt_resolved": true,
                "permission_mode": "workspace_write",
                "model_selection": {
                    "provider_id": "deepseek",
                    "model_id": "deepseek-v4-pro",
                    "reasoning": "max"
                }
            }
        }))
        .expect("wire message");
        assert!(matches!(
            app_command(selected, &app),
            SessionCommand::StartTurn {
                permissions: PermissionProfile {
                    mode: PermissionMode::WorkspaceWrite,
                    ..
                },
                model_selection: Some(ModelSelection {
                    reasoning: ReasoningLevel::Max,
                    ..
                }),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn workspace_adapter_keeps_managed_settings_in_memory() {
        let options = test_options();
        let model_store = options.model_store_path.clone();
        let server = EmbeddedServer::new_workspace(options).expect("workspace server");
        let response = server
            .request("GET", "/api/model-settings", None)
            .await
            .expect("response");
        assert_eq!(response.status, 404);
        assert!(!model_store.exists());
    }
}
