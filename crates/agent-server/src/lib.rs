mod commands;
mod mcp_settings;
mod models;

pub use models::FallbackModel;

use agent_config::{ContextConfig, McpServerConfig};
use agent_model::ModelError;
use agent_protocol::{
    ApprovalDecision, ModelSelection, PermissionMode, PermissionProfile, Session, SessionDocument,
};
use agent_runtime::{
    AgentEventEnvelope, CancellationToken, McpInspection, McpToolCache, RunAgentTurnContext,
    SessionListingEntry, SessionStore, TurnEventHandler, inspect_mcp_servers,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use commands::{
    CommandRegistry, CommandRegistryError, CommandResponse, CommandSettingsResponse,
    CommandWriteRequest, ResolveCommandRequest, ResolveCommandResponse,
};
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use mcp_settings::{
    McpRegistry, McpRegistryError, McpServerResponse, McpServerTestRequest, McpServerWriteRequest,
    McpSettingsResponse,
};
use models::{
    DiscoverModelsRequest, DiscoverModelsResponse, ModelProviderResponse, ModelRegistry,
    ModelRegistryError, ModelSettingsResponse, ProviderWriteRequest, ResolvedModel,
    SessionModelSelectionResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, broadcast, oneshot};
use tokio::task::AbortHandle;

pub const DEFAULT_WEB_PERMISSION_MODE: PermissionMode = PermissionMode::WorkspaceWrite;

#[derive(Clone)]
pub struct ServerOptions {
    pub host: IpAddr,
    pub port: u16,
    pub fallback_model: Option<FallbackModel>,
    pub model_store_path: PathBuf,
    pub mcp_store_path: PathBuf,
    pub command_store_path: PathBuf,
    pub system_prompt: String,
    pub context_config: ContextConfig,
    pub workspace_root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub config_diagnostics: Vec<String>,
    /// Default for legacy clients that do not select a permission mode per turn.
    pub permissions: PermissionProfile,
    pub mcp_servers: Vec<McpServerConfig>,
    pub default_session_name: String,
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
}

pub async fn serve(options: ServerOptions) -> Result<(), ServerError> {
    let addr = SocketAddr::new(options.host, options.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    axum::serve(listener, router(options)?)
        .await
        .map_err(ServerError::Serve)
}

pub fn router(options: ServerOptions) -> Result<Router, ModelRegistryError> {
    let model_registry = ModelRegistry::load(
        options.model_store_path.clone(),
        &options.workspace_root,
        options.fallback_model.clone(),
    )?;
    let mcp_registry =
        McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
            .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
    let command_registry = CommandRegistry::new(options.command_store_path.clone());
    let state = AppState {
        inner: Arc::new(ServerState {
            options,
            model_registry,
            mcp_registry,
            command_registry,
            sessions: Mutex::new(HashMap::new()),
            mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
        }),
    };

    Ok(Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/api/status", get(status))
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
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/{name}",
            get(get_session).post(create_session),
        )
        .route("/api/sessions/{name}/reset", post(reset_session))
        .route("/api/sessions/{name}/archive", post(archive_session))
        .route("/api/sessions/{name}/restore", post(restore_session))
        .route(
            "/api/sessions/{name}/model-selection",
            get(get_session_model_selection).put(set_session_model_selection),
        )
        .route("/api/sessions/{name}/ws", get(session_ws))
        .with_state(state))
}

#[derive(Clone)]
struct AppState {
    inner: Arc<ServerState>,
}

struct ServerState {
    options: ServerOptions,
    model_registry: ModelRegistry,
    mcp_registry: McpRegistry,
    command_registry: CommandRegistry,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    mcp_cache: RwLock<Arc<McpToolCache>>,
}

struct SessionRuntime {
    tx: broadcast::Sender<ServerMessage>,
    running: Option<RunningTurn>,
}

struct RunningTurn {
    turn_id: String,
    pending_approval: Option<PendingApproval>,
    cancellation: CancellationToken,
    handle: AbortHandle,
}

struct PendingApproval {
    request_id: String,
    sender: oneshot::Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusResponse {
    workspace_root: String,
    config_path: Option<String>,
    permissions: PermissionProfile,
    version: &'static str,
    model_ready: bool,
    model_store_path: String,
    mcp_store_path: String,
    command_store_path: String,
    config_diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionEntryResponse {
    name: String,
    path: String,
    turns: usize,
    active_messages: usize,
    summarized_turns: usize,
    has_summary: bool,
    archived: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SessionArchiveResponse {
    name: String,
    archived: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RunningTurnSnapshot {
    turn_id: String,
    pending_approval: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ServerMessage {
    Snapshot {
        session: Session,
        running_turn: Option<RunningTurnSnapshot>,
        permissions: PermissionProfile,
    },
    AgentEvent(AgentEventEnvelope),
    TurnSaved {
        session: String,
        turn_index: usize,
    },
    TurnRejected {
        request_id: String,
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ClientMessage {
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
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
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

impl From<agent_runtime::RuntimeError> for ApiError {
    fn from(error: agent_runtime::RuntimeError) -> Self {
        Self::internal(error.to_string())
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn asset(Path(path): Path<String>) -> Response {
    match path.as_str() {
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

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let settings = state.inner.model_registry.settings().await;
    Json(StatusResponse {
        workspace_root: state.inner.options.workspace_root.display().to_string(),
        config_path: state
            .inner
            .options
            .config_path
            .as_ref()
            .map(|path| path.display().to_string()),
        permissions: state.inner.options.permissions,
        version: env!("CARGO_PKG_VERSION"),
        model_ready: settings.model_ready,
        model_store_path: settings.store_path,
        mcp_store_path: state.inner.options.mcp_store_path.display().to_string(),
        command_store_path: state.inner.command_registry.root().display().to_string(),
        config_diagnostics: state.inner.options.config_diagnostics.clone(),
    })
}

async fn model_settings(State(state): State<AppState>) -> Json<ModelSettingsResponse> {
    Json(state.inner.model_registry.settings().await)
}

async fn create_model_provider(
    State(state): State<AppState>,
    Json(request): Json<ProviderWriteRequest>,
) -> Result<Json<ModelProviderResponse>, ApiError> {
    state
        .inner
        .model_registry
        .create_provider(request)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

async fn update_model_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Json(request): Json<ProviderWriteRequest>,
) -> Result<Json<ModelProviderResponse>, ApiError> {
    state
        .inner
        .model_registry
        .update_provider(&provider_id, request)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

async fn delete_model_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .inner
        .model_registry
        .delete_provider(&provider_id)
        .await
        .map_err(model_registry_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn discover_model_provider(
    State(state): State<AppState>,
    Json(request): Json<DiscoverModelsRequest>,
) -> Result<Json<DiscoverModelsResponse>, ApiError> {
    state
        .inner
        .model_registry
        .discover(request)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

async fn set_default_model(
    State(state): State<AppState>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<ModelSelection>, ApiError> {
    state
        .inner
        .model_registry
        .set_default(selection)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

async fn mcp_settings(State(state): State<AppState>) -> Json<McpSettingsResponse> {
    Json(state.inner.mcp_registry.settings().await)
}

async fn create_mcp_server(
    State(state): State<AppState>,
    Json(request): Json<McpServerWriteRequest>,
) -> Result<Json<McpServerResponse>, ApiError> {
    let response = state
        .inner
        .mcp_registry
        .create(request)
        .await
        .map_err(mcp_registry_error)?;
    reset_mcp_cache(&state).await;
    Ok(Json(response))
}

async fn update_mcp_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<McpServerWriteRequest>,
) -> Result<Json<McpServerResponse>, ApiError> {
    let response = state
        .inner
        .mcp_registry
        .update(&name, request)
        .await
        .map_err(mcp_registry_error)?;
    reset_mcp_cache(&state).await;
    Ok(Json(response))
}

async fn delete_mcp_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .inner
        .mcp_registry
        .delete(&name)
        .await
        .map_err(mcp_registry_error)?;
    reset_mcp_cache(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn import_mcp_servers(
    State(state): State<AppState>,
    Json(value): Json<serde_json::Value>,
) -> Result<Json<Vec<McpServerResponse>>, ApiError> {
    let response = state
        .inner
        .mcp_registry
        .import(value)
        .await
        .map_err(mcp_registry_error)?;
    reset_mcp_cache(&state).await;
    Ok(Json(response))
}

async fn test_mcp_server(
    State(state): State<AppState>,
    Json(request): Json<McpServerTestRequest>,
) -> Result<Json<McpInspection>, ApiError> {
    let server = state
        .inner
        .mcp_registry
        .config_for_test(request)
        .await
        .map_err(mcp_registry_error)?;
    Ok(Json(
        inspect_mcp_servers(&state.inner.options.workspace_root, &[server]).await,
    ))
}

async fn command_settings(
    State(state): State<AppState>,
) -> Result<Json<CommandSettingsResponse>, ApiError> {
    state
        .inner
        .command_registry
        .settings()
        .map(Json)
        .map_err(command_registry_error)
}

async fn create_command(
    State(state): State<AppState>,
    Json(request): Json<CommandWriteRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    state
        .inner
        .command_registry
        .create(request)
        .await
        .map(Json)
        .map_err(command_registry_error)
}

async fn update_command(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandWriteRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    state
        .inner
        .command_registry
        .update(&name, request)
        .await
        .map(Json)
        .map_err(command_registry_error)
}

async fn delete_command(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .inner
        .command_registry
        .delete(&name)
        .await
        .map_err(command_registry_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resolve_command(
    State(state): State<AppState>,
    Json(request): Json<ResolveCommandRequest>,
) -> Result<Json<ResolveCommandResponse>, ApiError> {
    state
        .inner
        .command_registry
        .resolve(request)
        .map(Json)
        .map_err(command_registry_error)
}

async fn get_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    session_store(&name)?;
    Ok(Json(
        state.inner.model_registry.session_selection(&name).await,
    ))
}

async fn set_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    session_store(&name)?;
    state
        .inner
        .model_registry
        .set_session_selection(&name, selection)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<SessionEntryResponse>>, ApiError> {
    let store = SessionStore::for_current_dir(&state.inner.options.default_session_name)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let entries = store
        .list_current_scope_with_archived()
        .map_err(|error| ApiError::internal(error.to_string()))?
        .into_iter()
        .map(session_entry_response)
        .collect();

    Ok(Json(entries))
}

async fn get_session(
    State(_state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    let store = session_store(&name)?;
    reject_archived_session(&store, &name)?;
    Ok(Json(SessionDocument::new(
        store
            .load()
            .map_err(|error| ApiError::internal(error.to_string()))?,
    )))
}

async fn create_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    if running_snapshot(&state, &name).await.is_some() {
        return Err(ApiError::conflict("session has a running turn"));
    }

    let store = session_store(&name)?;
    if store.is_archived() {
        return Err(ApiError::conflict(format!(
            "session {name:?} is archived; restore it before creating a session with the same name"
        )));
    }
    match store.load_existing() {
        Ok(_) => {
            return Err(ApiError::conflict(format!(
                "session {name:?} already exists"
            )));
        }
        Err(agent_runtime::SessionStoreError::SessionNotFound { .. }) => {}
        Err(error) => return Err(ApiError::internal(error.to_string())),
    }

    let session = Session::new();
    store
        .save(&session)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(SessionDocument::new(session)))
}

async fn reset_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    if running_snapshot(&state, &name).await.is_some() {
        return Err(ApiError::conflict("session has a running turn"));
    }

    let store = session_store(&name)?;
    reject_archived_session(&store, &name)?;
    let session = Session::new();
    store
        .save(&session)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(SessionDocument::new(session)))
}

async fn archive_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionArchiveResponse>, ApiError> {
    if running_snapshot(&state, &name).await.is_some() {
        return Err(ApiError::conflict("session has a running turn"));
    }

    let store = session_store(&name)?;
    store.archive().map_err(session_mutation_error)?;
    Ok(Json(SessionArchiveResponse {
        name,
        archived: true,
    }))
}

async fn restore_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionArchiveResponse>, ApiError> {
    if running_snapshot(&state, &name).await.is_some() {
        return Err(ApiError::conflict("session has a running turn"));
    }

    let store = session_store(&name)?;
    store.restore().map_err(session_mutation_error)?;
    Ok(Json(SessionArchiveResponse {
        name,
        archived: false,
    }))
}

async fn session_ws(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, name))
}

async fn handle_socket(socket: WebSocket, state: AppState, session_name: String) {
    let tx = session_sender(&state, &session_name).await;
    let mut rx = tx.subscribe();
    let (mut sender, mut receiver) = socket.split();

    match snapshot_message(&state, &session_name).await {
        Ok(snapshot) => {
            if send_server_message(&mut sender, &snapshot).await.is_err() {
                return;
            }
        }
        Err(error) => {
            let _ = send_server_message(
                &mut sender,
                &ServerMessage::Error {
                    message: error.message,
                },
            )
            .await;
            return;
        }
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                if !handle_client_ws_message(message, &state, &session_name, &tx).await {
                    break;
                }
            }
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(message) => {
                        if send_server_message(&mut sender, &message).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn send_server_message(
    sender: &mut SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

async fn handle_client_ws_message(
    message: Message,
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
) -> bool {
    let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => return false,
        _ => return true,
    };

    let parsed = serde_json::from_str::<ClientMessage>(&text);
    let Ok(message) = parsed else {
        broadcast_error(tx, "invalid websocket message");
        return true;
    };

    match message {
        ClientMessage::StartTurn {
            request_id,
            prompt,
            prompt_resolved,
            permission_mode,
            model_selection,
        } => {
            start_turn(
                state.clone(),
                session_name.to_string(),
                StartTurnRequest {
                    request_id,
                    prompt,
                    prompt_resolved,
                    permission_mode,
                    model_selection,
                },
                tx.clone(),
            )
            .await;
        }
        ClientMessage::ApprovalDecision {
            request_id,
            approved,
        } => {
            resolve_approval(state, session_name, request_id, approved, tx).await;
        }
        ClientMessage::CancelTurn { turn_id } => {
            cancel_turn(state, session_name, turn_id, tx).await;
        }
    }

    true
}

struct StartTurnRequest {
    request_id: String,
    prompt: String,
    prompt_resolved: bool,
    permission_mode: Option<PermissionMode>,
    model_selection: Option<ModelSelection>,
}

async fn start_turn(
    state: AppState,
    session_name: String,
    request: StartTurnRequest,
    tx: broadcast::Sender<ServerMessage>,
) {
    let StartTurnRequest {
        request_id,
        prompt,
        prompt_resolved,
        permission_mode,
        model_selection,
    } = request;
    let prompt = if prompt_resolved {
        prompt
    } else {
        match state
            .inner
            .command_registry
            .resolve(ResolveCommandRequest { input: prompt })
        {
            Ok(resolved) => resolved.prompt,
            Err(error) => {
                broadcast_message(
                    &tx,
                    ServerMessage::TurnRejected {
                        request_id,
                        reason: error.to_string(),
                    },
                );
                return;
            }
        }
    };
    if prompt.trim().is_empty() {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: "prompt must not be empty".to_string(),
            },
        );
        return;
    }
    let store = match session_store(&session_name) {
        Ok(store) => store,
        Err(error) => {
            broadcast_message(
                &tx,
                ServerMessage::TurnRejected {
                    request_id,
                    reason: error.message,
                },
            );
            return;
        }
    };
    if store.is_archived() {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: format!(
                    "session {session_name:?} is archived; restore it before starting a turn"
                ),
            },
        );
        return;
    }

    let turn_id = format!("turn-{}", agent_runtime::timestamp_ms());
    let cancellation = CancellationToken::new();
    let permissions = requested_permissions(state.inner.options.permissions, permission_mode);
    if running_snapshot(&state, &session_name).await.is_some() {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: "session already has a running turn".to_string(),
            },
        );
        return;
    }
    let resolved_model = match state
        .inner
        .model_registry
        .resolve_for_turn(&session_name, model_selection)
        .await
    {
        Ok(model) => model,
        Err(error) => {
            broadcast_message(
                &tx,
                ServerMessage::TurnRejected {
                    request_id,
                    reason: error.to_string(),
                },
            );
            return;
        }
    };
    if let Err(error) = state
        .inner
        .model_registry
        .set_session_selection(&session_name, resolved_model.selection.clone())
        .await
    {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: error.to_string(),
            },
        );
        return;
    }
    {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry(session_name.clone())
            .or_insert_with(SessionRuntime::new);
        if runtime.running.is_some() {
            broadcast_message(
                &tx,
                ServerMessage::TurnRejected {
                    request_id,
                    reason: "session already has a running turn".to_string(),
                },
            );
            return;
        }
        let state_for_task = state.clone();
        let session_for_task = session_name.clone();
        let turn_for_task = turn_id.clone();
        let cancellation_for_task = cancellation.clone();
        let tx_for_task = tx.clone();
        let worker = tokio::spawn(async move {
            run_turn_task(TurnTaskContext {
                state: state_for_task,
                session_name: session_for_task,
                turn_id: turn_for_task,
                prompt,
                permissions,
                resolved_model,
                tx: tx_for_task,
                cancellation: cancellation_for_task,
            })
            .await;
        });
        let handle = worker.abort_handle();
        let state_for_supervisor = state.clone();
        let session_for_supervisor = session_name.clone();
        let turn_for_supervisor = turn_id.clone();
        let tx_for_supervisor = tx.clone();
        tokio::spawn(supervise_turn_worker(
            state_for_supervisor,
            session_for_supervisor,
            turn_for_supervisor,
            tx_for_supervisor,
            worker,
        ));
        runtime.running = Some(RunningTurn {
            turn_id: turn_id.clone(),
            pending_approval: None,
            cancellation,
            handle,
        });
    }

    if let Ok(snapshot) = snapshot_message(&state, &session_name).await {
        broadcast_message(&tx, snapshot);
    }
}

struct TurnTaskContext {
    state: AppState,
    session_name: String,
    turn_id: String,
    prompt: String,
    permissions: PermissionProfile,
    resolved_model: ResolvedModel,
    tx: broadcast::Sender<ServerMessage>,
    cancellation: CancellationToken,
}

async fn run_turn_task(context: TurnTaskContext) {
    let tx = context.tx.clone();
    let result = run_turn_task_inner(context).await;
    if let Err(error) = result {
        broadcast_error(&tx, error.to_string());
    }
}

async fn supervise_turn_worker(
    state: AppState,
    session_name: String,
    turn_id: String,
    tx: broadcast::Sender<ServerMessage>,
    worker: tokio::task::JoinHandle<()>,
) {
    if worker.await.is_err_and(|error| error.is_panic()) {
        broadcast_error(&tx, format!("turn {turn_id} worker panicked"));
    }
    // 无论正常返回、panic 还是 abort，JoinHandle 完成都表示 worker future 已被 drop。
    clear_running_turn(&state, &session_name, &turn_id).await;
}

async fn run_turn_task_inner(context: TurnTaskContext) -> Result<(), agent_runtime::RuntimeError> {
    let TurnTaskContext {
        state,
        session_name,
        turn_id,
        prompt,
        permissions,
        resolved_model,
        tx,
        cancellation,
    } = context;
    let options = state.inner.options.clone();
    let mcp_cache = state.inner.mcp_cache.read().await.clone();
    let mcp_servers = state.inner.mcp_registry.effective_servers().await;
    let store = SessionStore::for_current_dir(&session_name)?;
    let mut session = store.load()?;
    let turn_index = session.turns.len();
    let mut handler = ServerTurnHandler {
        state: state.clone(),
        session_name: session_name.clone(),
        turn_id,
        tx: tx.clone(),
    };

    let outcome = agent_runtime::run_agent_turn_with_cancellation(
        RunAgentTurnContext {
            client: &resolved_model.client,
            system_prompt: &options.system_prompt,
            context_config: options.context_config,
            model_limits: resolved_model.limits,
            workspace_root: &options.workspace_root,
            permissions,
            mcp_servers: &mcp_servers,
            mcp_cache: mcp_cache.as_ref(),
            session_name: &session_name,
            turn_index,
        },
        &mut session,
        &prompt,
        &mut handler,
        cancellation,
    )
    .await?;

    if let Some(record) = session.turns.get_mut(turn_index) {
        record.turn.model = Some(resolved_model.invocation);
    }

    if outcome.session_changed {
        store.save(&session)?;
        broadcast_message(
            &tx,
            ServerMessage::TurnSaved {
                session: session_name,
                turn_index,
            },
        );
    }
    if let Some(error) = outcome.error {
        broadcast_error(&tx, error);
    }

    Ok(())
}

async fn resolve_approval(
    state: &AppState,
    session_name: &str,
    request_id: String,
    approved: bool,
    tx: &broadcast::Sender<ServerMessage>,
) {
    let pending = {
        let mut sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get_mut(session_name) else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        let Some(running) = runtime.running.as_mut() else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        let Some(pending) = running.pending_approval.take() else {
            broadcast_error(tx, "session has no pending approval");
            return;
        };
        if pending.request_id != request_id {
            let expected = pending.request_id.clone();
            running.pending_approval = Some(pending);
            broadcast_error(
                tx,
                format!(
                    "approval decision {request_id} does not match pending approval {expected}"
                ),
            );
            return;
        }
        pending
    };

    let _ = pending.sender.send(if approved {
        ApprovalDecision::approve(request_id)
    } else {
        ApprovalDecision::deny(request_id)
    });
}

async fn cancel_turn(
    state: &AppState,
    session_name: &str,
    turn_id: String,
    tx: &broadcast::Sender<ServerMessage>,
) {
    let cancellation = {
        let sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get(session_name) else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        let Some(running) = runtime.running.as_ref() else {
            broadcast_error(tx, "session has no running turn");
            return;
        };
        if running.turn_id != turn_id {
            broadcast_error(tx, format!("turn {turn_id} is not running"));
            return;
        }
        running.cancellation.clone()
    };

    cancellation.cancel();

    // 正常路径由 runtime 收束失败 Turn。只有长期不退出时才使用 abort 兜底。
    let state = state.clone();
    let session_name = session_name.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let handle = {
            let sessions = state.inner.sessions.lock().await;
            sessions
                .get(&session_name)
                .and_then(|runtime| runtime.running.as_ref())
                .filter(|running| running.turn_id == turn_id && running.cancellation.is_cancelled())
                .map(|running| running.handle.clone())
        };
        if let Some(handle) = handle {
            handle.abort();
            // `abort` 只发送终止请求。等待任务真正结束，确保其 future（以及工具清理
            // guard）已被 drop 后，才允许同一 Session 接受下一轮请求。
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
            clear_running_turn(&state, &session_name, &turn_id).await;
            broadcast_error(&tx, format!("turn {turn_id} cancellation timed out"));
        }
    });
}

async fn clear_running_turn(state: &AppState, session_name: &str, turn_id: &str) {
    let mut sessions = state.inner.sessions.lock().await;
    if let Some(runtime) = sessions.get_mut(session_name)
        && runtime
            .running
            .as_ref()
            .is_some_and(|running| running.turn_id == turn_id)
    {
        runtime.running = None;
    }
}

async fn reset_mcp_cache(state: &AppState) {
    let previous = {
        let mut current = state.inner.mcp_cache.write().await;
        std::mem::replace(&mut *current, Arc::new(McpToolCache::new()))
    };
    previous.clear().await;
}

struct ServerTurnHandler {
    state: AppState,
    session_name: String,
    turn_id: String,
    tx: broadcast::Sender<ServerMessage>,
}

impl TurnEventHandler for ServerTurnHandler {
    fn on_event(
        &mut self,
        envelope: &AgentEventEnvelope,
    ) -> Result<(), agent_runtime::RuntimeError> {
        broadcast_message(&self.tx, ServerMessage::AgentEvent(envelope.clone()));
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a agent_protocol::ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, agent_runtime::RuntimeError>> {
        let state = self.state.clone();
        let session_name = self.session_name.clone();
        let turn_id = self.turn_id.clone();
        let request_id = request.id.clone();

        async move {
            let (sender, receiver) = oneshot::channel();
            {
                let mut sessions = state.inner.sessions.lock().await;
                let runtime = sessions.get_mut(&session_name).ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("session state disappeared")
                })?;
                let running = runtime.running.as_mut().ok_or_else(|| {
                    agent_runtime::RuntimeError::event_handler("running turn disappeared")
                })?;
                if running.turn_id != turn_id {
                    return Err(agent_runtime::RuntimeError::event_handler(
                        "running turn changed while waiting for approval",
                    ));
                }
                running.pending_approval = Some(PendingApproval {
                    request_id: request_id.clone(),
                    sender,
                });
            }

            match receiver.await {
                Ok(decision) => Ok(decision),
                Err(_) => Ok(ApprovalDecision::deny(request_id)),
            }
        }
        .boxed()
    }
}

impl SessionRuntime {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx, running: None }
    }
}

async fn session_sender(state: &AppState, session_name: &str) -> broadcast::Sender<ServerMessage> {
    let mut sessions = state.inner.sessions.lock().await;
    sessions
        .entry(session_name.to_string())
        .or_insert_with(SessionRuntime::new)
        .tx
        .clone()
}

async fn snapshot_message(state: &AppState, session_name: &str) -> Result<ServerMessage, ApiError> {
    let store = session_store(session_name)?;
    reject_archived_session(&store, session_name)?;
    let session = store
        .load()
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(ServerMessage::Snapshot {
        session,
        running_turn: running_snapshot(state, session_name).await,
        permissions: state.inner.options.permissions,
    })
}

async fn running_snapshot(state: &AppState, session_name: &str) -> Option<RunningTurnSnapshot> {
    let sessions = state.inner.sessions.lock().await;
    sessions
        .get(session_name)
        .and_then(|runtime| runtime.running.as_ref())
        .map(|running| RunningTurnSnapshot {
            turn_id: running.turn_id.clone(),
            pending_approval: running
                .pending_approval
                .as_ref()
                .map(|approval| approval.request_id.clone()),
        })
}

fn session_store(name: &str) -> Result<SessionStore, ApiError> {
    SessionStore::for_current_dir(name).map_err(|error| ApiError::bad_request(error.to_string()))
}

fn requested_permissions(
    default: PermissionProfile,
    requested_mode: Option<PermissionMode>,
) -> PermissionProfile {
    requested_mode
        .map(PermissionProfile::for_mode)
        .unwrap_or(default)
}

fn reject_archived_session(store: &SessionStore, name: &str) -> Result<(), ApiError> {
    if store.is_archived() {
        return Err(ApiError::conflict(format!(
            "session {name:?} is archived; restore it before opening it"
        )));
    }
    Ok(())
}

fn session_mutation_error(error: agent_runtime::SessionStoreError) -> ApiError {
    match error {
        agent_runtime::SessionStoreError::SessionNotFound { .. }
        | agent_runtime::SessionStoreError::TargetExists { .. } => {
            ApiError::conflict(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}

fn model_registry_error(error: ModelRegistryError) -> ApiError {
    match error {
        ModelRegistryError::Conflict(_) | ModelRegistryError::SelectionUnavailable(_) => {
            ApiError::conflict(error.to_string())
        }
        ModelRegistryError::Validation(_) | ModelRegistryError::ProviderNotFound(_) => {
            ApiError::bad_request(error.to_string())
        }
        ModelRegistryError::Model(ModelError::HttpStatus { .. })
        | ModelRegistryError::Model(ModelError::Request(_)) => {
            ApiError::bad_request(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}

fn mcp_registry_error(error: McpRegistryError) -> ApiError {
    match error {
        McpRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        McpRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        McpRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}

fn command_registry_error(error: CommandRegistryError) -> ApiError {
    match error {
        CommandRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        CommandRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        CommandRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}

fn session_entry_response(entry: SessionListingEntry) -> SessionEntryResponse {
    let session = entry.session;
    SessionEntryResponse {
        name: session.name,
        path: session.path.display().to_string(),
        turns: session.turns,
        active_messages: session.active_messages,
        summarized_turns: session.summarized_turns,
        has_summary: session.has_summary,
        archived: entry.archived,
    }
}

fn broadcast_message(tx: &broadcast::Sender<ServerMessage>, message: ServerMessage) {
    let _ = tx.send(message);
}

fn broadcast_error(tx: &broadcast::Sender<ServerMessage>, message: impl ToString) {
    broadcast_message(
        tx,
        ServerMessage::Error {
            message: message.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_config::ModelContextLimits;
    use agent_model::{OpenAiCompatClient, OpenAiCompatConfig};
    use agent_protocol::{PermissionMode, ReasoningProfile, ShellPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

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
                client,
                limits: ModelContextLimits {
                    context_window_tokens: 65_536,
                    reserved_output_tokens: 8_192,
                },
                reasoning_profile: ReasoningProfile::None,
            }),
            model_store_path: root.join("web-models.json"),
            mcp_store_path: root.join("web-mcp.json"),
            command_store_path: root.join("commands"),
            system_prompt: "system".to_string(),
            context_config: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 2,
            },
            workspace_root: root.clone(),
            config_path: Some(root.join("morrow.toml")),
            config_diagnostics: Vec::new(),
            permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
            mcp_servers: Vec::new(),
            default_session_name: "default".to_string(),
        }
    }

    fn test_state() -> AppState {
        let options = test_options();
        let model_registry = ModelRegistry::load(
            options.model_store_path.clone(),
            &options.workspace_root,
            options.fallback_model.clone(),
        )
        .expect("model registry");
        let mcp_registry =
            McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
                .expect("MCP registry");
        let command_registry = CommandRegistry::new(options.command_store_path.clone());
        AppState {
            inner: Arc::new(ServerState {
                options,
                model_registry,
                mcp_registry,
                command_registry,
                sessions: Mutex::new(HashMap::new()),
                mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
            }),
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let stamp = agent_runtime::timestamp_ms();
        let path = std::env::temp_dir().join(format!(
            "morrow-server-{name}-{stamp}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[tokio::test]
    async fn status_response_omits_api_key() {
        let response = status(State(test_state())).await;
        let value = serde_json::to_value(response.0).expect("status json");

        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["permissions"]["mode"], "workspace_write");
        assert!(!value.to_string().contains("secret-test-key"));
    }

    #[test]
    fn router_registers_model_routes_without_conflicts() {
        let _ = router(test_options()).expect("router");
    }

    #[tokio::test]
    async fn reset_rejects_running_session() {
        let state = test_state();
        let worker = tokio::spawn(std::future::pending::<()>());
        {
            let mut sessions = state.inner.sessions.lock().await;
            let runtime = sessions
                .entry("default".to_string())
                .or_insert_with(SessionRuntime::new);
            runtime.running = Some(RunningTurn {
                turn_id: "turn-1".to_string(),
                pending_approval: None,
                cancellation: CancellationToken::new(),
                handle: worker.abort_handle(),
            });
        }

        let result = reset_session(State(state), Path("default".to_string())).await;

        assert!(matches!(
            result,
            Err(ApiError {
                status: StatusCode::CONFLICT,
                ..
            })
        ));
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn cancellation_keeps_session_reserved_until_worker_cleanup() {
        let state = test_state();
        let tx = session_sender(&state, "default").await;
        let worker = tokio::spawn(std::future::pending::<()>());
        {
            let mut sessions = state.inner.sessions.lock().await;
            let runtime = sessions
                .entry("default".to_string())
                .or_insert_with(SessionRuntime::new);
            runtime.running = Some(RunningTurn {
                turn_id: "turn-1".to_string(),
                pending_approval: None,
                cancellation: CancellationToken::new(),
                handle: worker.abort_handle(),
            });
        }

        cancel_turn(&state, "default", "turn-1".to_string(), &tx).await;

        let sessions = state.inner.sessions.lock().await;
        let running = sessions
            .get("default")
            .and_then(|runtime| runtime.running.as_ref())
            .expect("running turn remains reserved");
        assert!(running.cancellation.is_cancelled());
        assert!(
            !worker.is_finished(),
            "cooperative cancellation must not abort the worker immediately"
        );
        drop(sessions);

        clear_running_turn(&state, "default", "turn-1").await;
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn worker_panic_releases_session_slot() {
        let state = test_state();
        let tx = session_sender(&state, "default").await;
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let worker = tokio::spawn(async move {
            let _ = release_rx.await;
            panic!("test worker panic");
        });
        {
            let mut sessions = state.inner.sessions.lock().await;
            let runtime = sessions
                .entry("default".to_string())
                .or_insert_with(SessionRuntime::new);
            runtime.running = Some(RunningTurn {
                turn_id: "turn-panic".to_string(),
                pending_approval: None,
                cancellation: CancellationToken::new(),
                handle: worker.abort_handle(),
            });
        }

        let supervisor = tokio::spawn(supervise_turn_worker(
            state.clone(),
            "default".to_string(),
            "turn-panic".to_string(),
            tx,
            worker,
        ));
        release_tx.send(()).expect("release worker");
        tokio::time::timeout(std::time::Duration::from_secs(1), supervisor)
            .await
            .expect("supervisor must finish")
            .expect("supervisor task");

        let sessions = state.inner.sessions.lock().await;
        assert!(
            sessions
                .get("default")
                .is_some_and(|runtime| runtime.running.is_none())
        );
    }

    #[tokio::test]
    async fn create_session_saves_empty_session() {
        let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
        let home = unique_test_dir("create-home");
        let cwd = unique_test_dir("create-cwd");
        let previous_cwd = std::env::current_dir().expect("current dir");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        std::env::set_current_dir(&cwd).expect("set cwd");

        let response = create_session(State(test_state()), Path("fresh".to_string()))
            .await
            .expect("create session");
        let store = SessionStore::for_current_dir("fresh").expect("store");
        let session = store.load_existing().expect("load created session");

        assert_eq!(response.0.session, Session::new());
        assert_eq!(session, Session::new());
        assert!(store.path().is_file());

        std::env::set_current_dir(previous_cwd).expect("restore cwd");
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        drop(lock);
    }

    #[tokio::test]
    async fn create_session_rejects_existing_session() {
        let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
        let home = unique_test_dir("create-existing-home");
        let cwd = unique_test_dir("create-existing-cwd");
        let previous_cwd = std::env::current_dir().expect("current dir");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        std::env::set_current_dir(&cwd).expect("set cwd");

        let store = SessionStore::for_current_dir("existing").expect("store");
        store.save(&Session::new()).expect("save existing session");

        let result = create_session(State(test_state()), Path("existing".to_string())).await;

        assert!(matches!(
            result,
            Err(ApiError {
                status: StatusCode::CONFLICT,
                ..
            })
        ));

        std::env::set_current_dir(previous_cwd).expect("restore cwd");
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        drop(lock);
    }

    #[tokio::test]
    async fn archive_and_restore_session_updates_session_listing() {
        let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
        let home = unique_test_dir("archive-home");
        let cwd = unique_test_dir("archive-cwd");
        let previous_cwd = std::env::current_dir().expect("current dir");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        std::env::set_current_dir(&cwd).expect("set cwd");

        let state = test_state();
        let store = SessionStore::for_current_dir("work").expect("store");
        store.save(&Session::new()).expect("save session");

        let archived = archive_session(State(state.clone()), Path("work".to_string()))
            .await
            .expect("archive session");
        let entries = list_sessions(State(state.clone()))
            .await
            .expect("list sessions");

        assert!(archived.0.archived);
        assert!(store.is_archived());
        assert_eq!(entries.0.len(), 1);
        assert!(entries.0[0].archived);

        let restored = restore_session(State(state), Path("work".to_string()))
            .await
            .expect("restore session");

        assert!(!restored.0.archived);
        assert!(!store.is_archived());
        assert!(store.load_existing().is_ok());

        std::env::set_current_dir(previous_cwd).expect("restore cwd");
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        drop(lock);
    }

    #[test]
    fn start_turn_message_accepts_optional_permission_mode() {
        let selected = serde_json::from_value::<ClientMessage>(json!({
            "type": "start_turn",
            "data": {
                "request_id": "request-1",
                "prompt": "edit the workspace",
                "prompt_resolved": true,
                "permission_mode": "workspace_write",
                "model_selection": {
                    "provider_id": "deepseek",
                    "model_id": "deepseek-v4-pro",
                    "reasoning": "max"
                }
            }
        }))
        .expect("parse selected permissions");
        let legacy = serde_json::from_value::<ClientMessage>(json!({
            "type": "start_turn",
            "data": {
                "request_id": "request-2",
                "prompt": "inspect the workspace"
            }
        }))
        .expect("parse legacy message");

        assert!(matches!(
            selected,
            ClientMessage::StartTurn {
                prompt_resolved: true,
                permission_mode: Some(PermissionMode::WorkspaceWrite),
                model_selection: Some(ModelSelection {
                    reasoning: agent_protocol::ReasoningLevel::Max,
                    ..
                }),
                ..
            }
        ));
        assert!(matches!(
            legacy,
            ClientMessage::StartTurn {
                prompt_resolved: false,
                permission_mode: None,
                ..
            }
        ));
    }

    #[test]
    fn requested_permissions_allow_all_web_modes() {
        let read_only = PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        };

        assert_eq!(
            requested_permissions(read_only, Some(PermissionMode::WorkspaceWrite)),
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite)
        );
        assert_eq!(
            requested_permissions(read_only, Some(PermissionMode::DangerFullAccess)),
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess)
        );
        assert_eq!(requested_permissions(read_only, None), read_only);
        assert_eq!(DEFAULT_WEB_PERMISSION_MODE, PermissionMode::WorkspaceWrite);
    }

    #[tokio::test]
    async fn wrong_approval_request_id_is_rejected() {
        let state = test_state();
        let tx = session_sender(&state, "default").await;
        let mut rx = tx.subscribe();
        let worker = tokio::spawn(std::future::pending::<()>());
        {
            let (sender, _receiver) = oneshot::channel();
            let mut sessions = state.inner.sessions.lock().await;
            let runtime = sessions
                .entry("default".to_string())
                .or_insert_with(SessionRuntime::new);
            runtime.running = Some(RunningTurn {
                turn_id: "turn-1".to_string(),
                pending_approval: Some(PendingApproval {
                    request_id: "approval-call_1".to_string(),
                    sender,
                }),
                cancellation: CancellationToken::new(),
                handle: worker.abort_handle(),
            });
        }

        resolve_approval(&state, "default", "approval-wrong".to_string(), true, &tx).await;

        let message = rx.recv().await.expect("error message");
        assert!(matches!(message, ServerMessage::Error { .. }));
        assert!(
            running_snapshot(&state, "default")
                .await
                .expect("running")
                .pending_approval
                .is_some()
        );
        clear_running_turn(&state, "default", "turn-1").await;
        worker.abort();
        let _ = worker.await;
    }
}
