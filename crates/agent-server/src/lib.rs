use agent_config::{ContextConfig, ModelContextLimits};
use agent_model::OpenAiCompatClient;
use agent_protocol::{ApprovalDecision, PermissionProfile, Session, SessionDocument};
use agent_runtime::{
    AgentEventEnvelope, RunAgentTurnContext, SessionEntry, SessionStore, TurnEventHandler,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct ServerOptions {
    pub host: IpAddr,
    pub port: u16,
    pub client: OpenAiCompatClient,
    pub system_prompt: String,
    pub context_config: ContextConfig,
    pub model_limits: ModelContextLimits,
    pub workspace_root: PathBuf,
    pub config_path: PathBuf,
    pub permissions: PermissionProfile,
    pub default_session_name: String,
}

#[derive(Debug, Error)]
pub enum ServerError {
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
    axum::serve(listener, router(options))
        .await
        .map_err(ServerError::Serve)
}

pub fn router(options: ServerOptions) -> Router {
    let state = AppState {
        inner: Arc::new(ServerState {
            options,
            sessions: Mutex::new(HashMap::new()),
        }),
    };

    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/api/status", get(status))
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/{name}",
            get(get_session).post(create_session),
        )
        .route("/api/sessions/{name}/reset", post(reset_session))
        .route("/api/sessions/{name}/ws", get(session_ws))
        .with_state(state)
}

#[derive(Clone)]
struct AppState {
    inner: Arc<ServerState>,
}

struct ServerState {
    options: ServerOptions,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
}

struct SessionRuntime {
    tx: broadcast::Sender<ServerMessage>,
    running: Option<RunningTurn>,
}

struct RunningTurn {
    turn_id: String,
    pending_approval: Option<PendingApproval>,
    handle: Option<JoinHandle<()>>,
}

struct PendingApproval {
    request_id: String,
    sender: oneshot::Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusResponse {
    workspace_root: String,
    config_path: String,
    permissions: PermissionProfile,
    version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct SessionEntryResponse {
    name: String,
    path: String,
    turns: usize,
    active_messages: usize,
    summarized_turns: usize,
    has_summary: bool,
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
    StartTurn { request_id: String, prompt: String },
    ApprovalDecision { request_id: String, approved: bool },
    CancelTurn { turn_id: String },
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
    Json(StatusResponse {
        workspace_root: state.inner.options.workspace_root.display().to_string(),
        config_path: state.inner.options.config_path.display().to_string(),
        permissions: state.inner.options.permissions,
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<SessionEntryResponse>>, ApiError> {
    let store = SessionStore::for_current_dir(&state.inner.options.default_session_name)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let entries = store
        .list_current_scope()
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
    let session = Session::new();
    store
        .save(&session)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(SessionDocument::new(session)))
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
        ClientMessage::StartTurn { request_id, prompt } => {
            start_turn(
                state.clone(),
                session_name.to_string(),
                request_id,
                prompt,
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

async fn start_turn(
    state: AppState,
    session_name: String,
    request_id: String,
    prompt: String,
    tx: broadcast::Sender<ServerMessage>,
) {
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
    if let Err(error) = session_store(&session_name) {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: error.message,
            },
        );
        return;
    }

    let turn_id = format!("turn-{}", agent_runtime::timestamp_ms());
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
        runtime.running = Some(RunningTurn {
            turn_id: turn_id.clone(),
            pending_approval: None,
            handle: None,
        });
    }

    if let Ok(snapshot) = snapshot_message(&state, &session_name).await {
        broadcast_message(&tx, snapshot);
    }

    let state_for_task = state.clone();
    let session_for_task = session_name.clone();
    let turn_for_task = turn_id.clone();
    let prompt_for_task = prompt;
    let tx_for_task = tx.clone();
    let handle = tokio::spawn(async move {
        run_turn_task(
            state_for_task,
            session_for_task,
            turn_for_task,
            prompt_for_task,
            tx_for_task,
        )
        .await;
    });

    let mut sessions = state.inner.sessions.lock().await;
    if let Some(runtime) = sessions.get_mut(&session_name)
        && let Some(running) = runtime.running.as_mut()
        && running.turn_id == turn_id
    {
        running.handle = Some(handle);
    }
}

async fn run_turn_task(
    state: AppState,
    session_name: String,
    turn_id: String,
    prompt: String,
    tx: broadcast::Sender<ServerMessage>,
) {
    let result = run_turn_task_inner(
        state.clone(),
        session_name.clone(),
        turn_id.clone(),
        prompt,
        tx.clone(),
    )
    .await;
    if let Err(error) = result {
        broadcast_error(&tx, error.to_string());
    }
    clear_running_turn(&state, &session_name, &turn_id).await;
}

async fn run_turn_task_inner(
    state: AppState,
    session_name: String,
    turn_id: String,
    prompt: String,
    tx: broadcast::Sender<ServerMessage>,
) -> Result<(), agent_runtime::RuntimeError> {
    let options = state.inner.options.clone();
    let store = SessionStore::for_current_dir(&session_name)?;
    let mut session = store.load()?;
    let turn_index = session.turns.len();
    let mut handler = ServerTurnHandler {
        state: state.clone(),
        session_name: session_name.clone(),
        turn_id,
        tx: tx.clone(),
    };

    let outcome = agent_runtime::run_agent_turn(
        RunAgentTurnContext {
            client: &options.client,
            system_prompt: &options.system_prompt,
            context_config: options.context_config,
            model_limits: options.model_limits,
            workspace_root: &options.workspace_root,
            permissions: options.permissions,
            session_name: &session_name,
            turn_index,
        },
        &mut session,
        &prompt,
        &mut handler,
    )
    .await?;

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
    let running = {
        let mut sessions = state.inner.sessions.lock().await;
        let Some(runtime) = sessions.get_mut(session_name) else {
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
        runtime.running.take()
    };

    if let Some(running) = running {
        if let Some(handle) = running.handle {
            handle.abort();
        }
        broadcast_error(tx, format!("turn {turn_id} cancelled"));
    }
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

fn session_entry_response(entry: SessionEntry) -> SessionEntryResponse {
    SessionEntryResponse {
        name: entry.name,
        path: entry.path.display().to_string(),
        turns: entry.turns,
        active_messages: entry.active_messages,
        summarized_turns: entry.summarized_turns,
        has_summary: entry.has_summary,
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
    use agent_model::OpenAiCompatConfig;
    use agent_protocol::{PermissionMode, ShellPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

    fn test_options() -> ServerOptions {
        let root = std::env::temp_dir();
        ServerOptions {
            host: "127.0.0.1".parse().expect("host"),
            port: 0,
            client: OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
                base_url: "http://127.0.0.1:1/v1".to_string(),
                model: "test-model".to_string(),
                api_key: "secret-test-key".to_string(),
                timeout: Duration::from_secs(1),
            })
            .expect("client"),
            system_prompt: "system".to_string(),
            context_config: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 2,
            },
            model_limits: ModelContextLimits {
                context_window_tokens: 65_536,
                reserved_output_tokens: 8_192,
            },
            workspace_root: root.clone(),
            config_path: root.join("morrow.toml"),
            permissions: PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Deny,
            },
            default_session_name: "default".to_string(),
        }
    }

    fn test_state() -> AppState {
        AppState {
            inner: Arc::new(ServerState {
                options: test_options(),
                sessions: Mutex::new(HashMap::new()),
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
        assert!(!value.to_string().contains("secret-test-key"));
    }

    #[tokio::test]
    async fn reset_rejects_running_session() {
        let state = test_state();
        {
            let mut sessions = state.inner.sessions.lock().await;
            let runtime = sessions
                .entry("default".to_string())
                .or_insert_with(SessionRuntime::new);
            runtime.running = Some(RunningTurn {
                turn_id: "turn-1".to_string(),
                pending_approval: None,
                handle: None,
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
    async fn wrong_approval_request_id_is_rejected() {
        let state = test_state();
        let tx = session_sender(&state, "default").await;
        let mut rx = tx.subscribe();
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
                handle: None,
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
    }
}
