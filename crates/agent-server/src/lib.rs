mod commands;
mod mcp_settings;
mod models;
mod secrets;
mod subagent_settings;

pub use models::{FallbackModel, discover_models as discover_remote_models};
pub use subagent_settings::{
    SubagentProfileResponse, SubagentProfileWriteRequest, SubagentRegistryError,
    SubagentSettingsResponse, load_subagent_identities,
};

use agent_config::{ContextConfig, LoadedServerConfig, McpServerConfig};
use agent_model::{ModelError, OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{
    ApprovalDecision, ModelSelection, PermissionMode, PermissionProfile, ReasoningProfile,
    RemoteMcpServerSpec, RemoteModelConnectionSpec, RemoteModelSpec, RemoteTurnModel,
    RemoteTurnSpec, Session, SessionDocument, SubagentIdentity, WorkspaceLocation,
};
use agent_runtime::{
    AgentEventEnvelope, CancellationToken, McpInspection, McpToolCache, RunAgentTurnContext,
    SessionListingEntry, SessionStore, TurnEventHandler, inspect_mcp_servers,
};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
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
    McpSettingsResponse, config_from_remote_spec, remote_spec_from_config,
};
use models::{
    DiscoverModelsRequest, DiscoverModelsResponse, ModelProviderResponse, ModelRegistry,
    ModelRegistryError, ModelSettingsResponse, ProviderWriteRequest, ResolvedModel,
    SessionModelSelectionResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use subagent_settings::SubagentRegistry;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, broadcast, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
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

pub fn server_options_from_loaded_config(
    host: IpAddr,
    port: u16,
    workspace_root: PathBuf,
    home: &std::path::Path,
    loaded: LoadedServerConfig,
    default_session_name: String,
) -> Result<ServerOptions, ModelError> {
    let fallback_model = loaded
        .model
        .map(|model| {
            let model_name = model.config.model.clone();
            let limits = model.config.context_limits();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: model.config.base_url,
                model: model_name.clone(),
                api_key: model.api_key,
                timeout: Duration::from_secs(model.config.timeout_secs),
            })?;
            Ok(FallbackModel {
                provider_name: "默认配置".to_string(),
                model_id: model_name.clone(),
                model_name: model_name.clone(),
                client: Some(client),
                limits,
                reasoning_profile: reasoning_profile(&model_name),
            })
        })
        .transpose()?;

    let workspace_location = WorkspaceLocation::Local {
        path: workspace_root.clone(),
    };
    Ok(ServerOptions {
        host,
        port,
        fallback_model,
        model_store_path: home.join(".morrow").join("web-models.json"),
        mcp_store_path: home.join(".morrow").join("web-mcp.json"),
        command_store_path: home.join(".morrow").join("commands"),
        subagent_store_path: home.join(".morrow").join("subagents.json"),
        system_prompt: loaded.config.agent.system_prompt,
        context_config: loaded.config.context,
        workspace_root,
        workspace_location,
        config_path: loaded.path,
        config_diagnostics: loaded.diagnostics,
        permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
        mcp_servers: loaded.config.mcp_servers,
        default_session_name,
    })
}

fn reasoning_profile(model: &str) -> ReasoningProfile {
    match model {
        "deepseek-v4-flash" | "deepseek-v4-pro" => ReasoningProfile::Deepseek,
        _ => ReasoningProfile::None,
    }
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
    receiver: broadcast::Receiver<ServerMessage>,
}

impl EmbeddedSessionSubscription {
    pub async fn recv(&mut self) -> Result<serde_json::Value, String> {
        loop {
            match self.receiver.recv().await {
                Ok(message) => {
                    return serde_json::to_value(message).map_err(|error| error.to_string());
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err("session event stream closed".to_string());
                }
            }
        }
    }
}

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
        let prompt = if prompt_resolved {
            prompt
        } else {
            self.service
                .inner
                .command_registry
                .resolve(ResolveCommandRequest { input: prompt })
                .map_err(|error| error.to_string())?
                .prompt
        };
        if prompt.trim().is_empty() {
            return Err("prompt must not be empty".to_string());
        }
        let model = self
            .service
            .inner
            .model_registry
            .resolve_remote_for_turn(session_name, model_selection)
            .await
            .map_err(|error| error.to_string())?;
        let selection = match &model {
            RemoteTurnModel::WorkspaceFallback { selection } => selection.clone(),
            RemoteTurnModel::Managed(spec) => ModelSelection {
                provider_id: spec.invocation.provider_id.clone(),
                model_id: spec.invocation.model_id.clone(),
                reasoning: spec.invocation.reasoning,
            },
        };
        self.service
            .inner
            .model_registry
            .set_session_selection(session_name, selection)
            .await
            .map_err(|error| error.to_string())?;
        let managed_mcp_servers = self
            .service
            .inner
            .mcp_registry
            .managed_servers()
            .await
            .iter()
            .map(remote_spec_from_config)
            .collect();
        let subagent_identities = self.service.inner.subagent_registry.identities().await;
        Ok(RemoteTurnSpec {
            session: session_name.to_string(),
            request_id,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
        })
    }

    pub async fn start_remote_turn(&self, turn: RemoteTurnSpec) -> Result<(), String> {
        let RemoteTurnSpec {
            session,
            request_id,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
        } = turn;
        let resolved_model = match model {
            RemoteTurnModel::WorkspaceFallback { selection } => self
                .service
                .inner
                .model_registry
                .resolve_for_turn(&session, Some(selection))
                .await
                .map_err(|error| error.to_string())?,
            RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
        };
        let mut mcp_servers = self.service.inner.mcp_registry.fallback_servers().to_vec();
        let mut names = mcp_servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<HashSet<_>>();
        for server in managed_mcp_servers {
            if !names.insert(server.name.clone()) {
                return Err(format!("duplicate MCP server name {:?}", server.name));
            }
            mcp_servers.push(config_from_remote_spec(server));
        }
        let tx = session_sender(&self.service, &session).await;
        start_turn(
            self.service.clone(),
            session,
            StartTurnRequest {
                request_id,
                prompt,
                prompt_resolved: true,
                permission_mode,
                model_selection: None,
                resolved_model: Some(resolved_model),
                mcp_servers: Some(mcp_servers),
                subagent_identities: Some(subagent_identities),
            },
            tx,
        )
        .await;
        Ok(())
    }

    pub async fn prepare_remote_model_discovery(
        &self,
        value: serde_json::Value,
    ) -> Result<RemoteModelConnectionSpec, String> {
        let request = serde_json::from_value::<DiscoverModelsRequest>(value)
            .map_err(|error| format!("invalid model discovery request: {error}"))?;
        self.service
            .inner
            .model_registry
            .discovery_spec(request)
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
            .inner
            .mcp_registry
            .config_for_test(request)
            .await
            .map(|server| remote_spec_from_config(&server))
            .map_err(|error| error.to_string())
    }

    pub async fn inspect_remote_mcp(&self, server: RemoteMcpServerSpec) -> McpInspection {
        inspect_mcp_servers(
            &self.service.inner.options.workspace_root,
            &[config_from_remote_spec(server)],
        )
        .await
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
        let tx = session_sender(self, session_name).await;
        let receiver = tx.subscribe();
        let snapshot = snapshot_message(self, session_name)
            .await
            .map_err(|error| error.message)?;
        Ok(EmbeddedSessionSubscription {
            snapshot: serde_json::to_value(snapshot).map_err(|error| error.to_string())?,
            receiver,
        })
    }

    pub async fn send_session_message(
        &self,
        session_name: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let message = serde_json::from_value::<ClientMessage>(value)
            .map_err(|error| format!("invalid session message: {error}"))?;
        let tx = session_sender(self, session_name).await;
        dispatch_client_message(message, self, session_name, &tx).await;
        Ok(())
    }

    pub async fn activity(&self) -> ServerActivity {
        server_activity(self).await
    }

    pub async fn shutdown(&self, cancel_running: bool) {
        self.inner.shutting_down.store(true, Ordering::Release);
        if cancel_running {
            cancel_all_turns(self, Duration::from_secs(5)).await;
        }
        reset_mcp_cache(self).await;
    }
}

fn resolved_model_from_remote(spec: RemoteModelSpec) -> Result<ResolvedModel, String> {
    if spec.context_window_tokens == 0
        || spec.reserved_output_tokens == 0
        || spec.reserved_output_tokens >= spec.context_window_tokens
    {
        return Err("remote model context limits are invalid".to_string());
    }
    if !(1..=600).contains(&spec.timeout_secs) {
        return Err("remote model timeout must be between 1 and 600 seconds".to_string());
    }
    let selection = ModelSelection {
        provider_id: spec.invocation.provider_id.clone(),
        model_id: spec.invocation.model_id.clone(),
        reasoning: spec.invocation.reasoning,
    };
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: spec.base_url,
        model: spec.model,
        api_key: spec.api_key,
        timeout: Duration::from_secs(spec.timeout_secs),
    })
    .map_err(|error| error.to_string())?
    .with_request_options(agent_model::OpenAiCompatRequestOptions {
        reasoning_profile: spec.reasoning_profile,
        reasoning: selection.reasoning,
        supports_tools: spec.supports_tools,
    });
    Ok(ResolvedModel {
        selection,
        invocation: spec.invocation,
        client,
        limits: agent_config::ModelContextLimits {
            context_window_tokens: spec.context_window_tokens,
            reserved_output_tokens: spec.reserved_output_tokens,
        },
    })
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn activity(&self) -> ServerActivity {
        server_activity(&self.state).await
    }

    pub async fn shutdown(&mut self, policy: ShutdownPolicy) -> Result<(), ServerError> {
        self.state
            .inner
            .shutting_down
            .store(true, Ordering::Release);
        let activity = server_activity(&self.state).await;
        match policy {
            ShutdownPolicy::RequireIdle if !activity.is_idle() => {
                self.state
                    .inner
                    .shutting_down
                    .store(false, Ordering::Release);
                return Err(ServerError::RunningTurns(activity.running_turns));
            }
            ShutdownPolicy::RequireIdle => {}
            ShutdownPolicy::CancelRunning { timeout } => {
                cancel_all_turns(&self.state, timeout).await;
            }
        }

        reset_mcp_cache(&self.state).await;
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
    let model_registry = if persistent_settings {
        ModelRegistry::load(
            options.model_store_path.clone(),
            &options.workspace_root,
            options.fallback_model.clone(),
        )?
    } else {
        ModelRegistry::in_memory(&options.workspace_root, options.fallback_model.clone())?
    };
    let mcp_registry = if persistent_settings {
        McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
    } else {
        McpRegistry::in_memory(options.mcp_servers.clone())
    }
    .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
    let command_registry = CommandRegistry::new(options.command_store_path.clone());
    let subagent_registry = if persistent_settings {
        SubagentRegistry::load(options.subagent_store_path.clone())
    } else {
        Ok(SubagentRegistry::in_memory(
            options.subagent_store_path.clone(),
        ))
    }
    .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
    let state = AppState {
        inner: Arc::new(ServerState {
            options,
            model_registry,
            mcp_registry,
            command_registry,
            subagent_registry,
            sessions: Mutex::new(HashMap::new()),
            mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
            access_policy,
            shutting_down: AtomicBool::new(false),
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

#[derive(Clone)]
pub struct WorkspaceService {
    inner: Arc<ServerState>,
}

type AppState = WorkspaceService;

struct ServerState {
    options: ServerOptions,
    model_registry: ModelRegistry,
    mcp_registry: McpRegistry,
    command_registry: CommandRegistry,
    subagent_registry: SubagentRegistry,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    mcp_cache: RwLock<Arc<McpToolCache>>,
    access_policy: ServerAccessPolicy,
    shutting_down: AtomicBool,
}

async fn access_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if matches!(state.inner.access_policy, ServerAccessPolicy::Embedded) {
        return next.run(request).await;
    }
    let response = match &state.inner.access_policy {
        ServerAccessPolicy::Browser => next.run(request).await,
        ServerAccessPolicy::Embedded => next.run(request).await,
        ServerAccessPolicy::Desktop { token } => {
            let expected_host =
                format!("{}:{}", state.inner.options.host, state.inner.options.port);
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

async fn server_activity(state: &AppState) -> ServerActivity {
    let sessions = state.inner.sessions.lock().await;
    let mut running_turns = 0;
    let mut pending_approvals = 0;
    for runtime in sessions.values() {
        if let Some(running) = runtime.running.as_ref() {
            running_turns += 1;
            pending_approvals += usize::from(running.pending_approval.is_some());
        }
    }
    ServerActivity {
        running_turns,
        pending_approvals,
    }
}

async fn cancel_all_turns(state: &AppState, timeout: Duration) {
    let handles = {
        let sessions = state.inner.sessions.lock().await;
        sessions
            .values()
            .filter_map(|runtime| runtime.running.as_ref())
            .map(|running| {
                running.cancellation.cancel();
                running.handle.clone()
            })
            .collect::<Vec<_>>()
    };

    let deadline = tokio::time::Instant::now() + timeout;
    while handles.iter().any(|handle| !handle.is_finished())
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for handle in handles.iter().filter(|handle| !handle.is_finished()) {
        handle.abort();
    }
    while handles.iter().any(|handle| !handle.is_finished()) {
        tokio::task::yield_now().await;
    }

    let mut sessions = state.inner.sessions.lock().await;
    for runtime in sessions.values_mut() {
        if runtime.running.is_some() {
            runtime.running = None;
        }
    }
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
    workspace_location: WorkspaceLocation,
    config_path: Option<String>,
    permissions: PermissionProfile,
    version: &'static str,
    model_ready: bool,
    model_store_path: String,
    mcp_store_path: String,
    command_store_path: String,
    subagent_store_path: String,
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
pub struct RunningTurnSnapshot {
    turn_id: String,
    pending_approval: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMessage {
    Snapshot {
        session: Session,
        running_turn: Option<RunningTurnSnapshot>,
        permissions: PermissionProfile,
    },
    AgentEvent(Box<AgentEventEnvelope>),
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

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let settings = state.inner.model_registry.settings().await;
    Json(StatusResponse {
        workspace_root: state.inner.options.workspace_root.display().to_string(),
        workspace_location: state.inner.options.workspace_location.clone(),
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
        subagent_store_path: state.inner.subagent_registry.path().display().to_string(),
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

async fn subagent_settings(State(state): State<AppState>) -> Json<SubagentSettingsResponse> {
    Json(state.inner.subagent_registry.settings().await)
}

async fn create_subagent(
    State(state): State<AppState>,
    Json(request): Json<SubagentProfileWriteRequest>,
) -> Result<Json<SubagentProfileResponse>, ApiError> {
    state
        .inner
        .subagent_registry
        .create(request)
        .await
        .map(Json)
        .map_err(subagent_registry_error)
}

async fn update_subagent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<SubagentProfileWriteRequest>,
) -> Result<Json<SubagentProfileResponse>, ApiError> {
    state
        .inner
        .subagent_registry
        .update(&id, request)
        .await
        .map(Json)
        .map_err(subagent_registry_error)
}

async fn delete_subagent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .inner
        .subagent_registry
        .delete(&id)
        .await
        .map_err(subagent_registry_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_subagents(
    State(state): State<AppState>,
) -> Result<Json<SubagentSettingsResponse>, ApiError> {
    state
        .inner
        .subagent_registry
        .reset()
        .await
        .map(Json)
        .map_err(subagent_registry_error)
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
    session_store(&state, &name)?;
    Ok(Json(
        state.inner.model_registry.session_selection(&name).await,
    ))
}

async fn set_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    session_store(&state, &name)?;
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
    let store = session_store(&state, &state.inner.options.default_session_name)?;
    let entries = store
        .list_current_scope_with_archived()
        .map_err(|error| ApiError::internal(error.to_string()))?
        .into_iter()
        .map(session_entry_response)
        .collect();

    Ok(Json(entries))
}

async fn get_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionDocument>, ApiError> {
    let store = session_store(&state, &name)?;
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

    let store = session_store(&state, &name)?;
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

    let store = session_store(&state, &name)?;
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

    let store = session_store(&state, &name)?;
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

    let store = session_store(&state, &name)?;
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

    dispatch_client_message(message, state, session_name, tx).await;
    true
}

async fn dispatch_client_message(
    message: ClientMessage,
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
) {
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
                    resolved_model: None,
                    mcp_servers: None,
                    subagent_identities: None,
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
}

struct StartTurnRequest {
    request_id: String,
    prompt: String,
    prompt_resolved: bool,
    permission_mode: Option<PermissionMode>,
    model_selection: Option<ModelSelection>,
    resolved_model: Option<ResolvedModel>,
    mcp_servers: Option<Vec<McpServerConfig>>,
    subagent_identities: Option<Vec<SubagentIdentity>>,
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
        resolved_model,
        mcp_servers,
        subagent_identities,
    } = request;
    if state.inner.shutting_down.load(Ordering::Acquire) {
        broadcast_message(
            &tx,
            ServerMessage::TurnRejected {
                request_id,
                reason: "server is shutting down".to_string(),
            },
        );
        return;
    }
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
    let store = match session_store(&state, &session_name) {
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
    let persist_model_selection = resolved_model.is_none();
    let resolved_model = match resolved_model {
        Some(model) => model,
        None => match state
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
        },
    };
    if persist_model_selection
        && let Err(error) = state
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
    let subagent_identities = match subagent_identities {
        Some(identities) if identities.len() >= subagent_settings::MIN_SUBAGENT_PROFILES => {
            identities
        }
        Some(_) => {
            broadcast_message(
                &tx,
                ServerMessage::TurnRejected {
                    request_id,
                    reason: format!(
                        "at least {} subagent identities are required",
                        subagent_settings::MIN_SUBAGENT_PROFILES
                    ),
                },
            );
            return;
        }
        None => state.inner.subagent_registry.identities().await,
    };
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
                mcp_servers,
                subagent_identities,
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
    mcp_servers: Option<Vec<McpServerConfig>>,
    subagent_identities: Vec<SubagentIdentity>,
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
        mcp_servers,
        subagent_identities,
        tx,
        cancellation,
    } = context;
    let options = state.inner.options.clone();
    let mcp_cache = state.inner.mcp_cache.read().await.clone();
    let mcp_servers = match mcp_servers {
        Some(servers) => servers,
        None => state.inner.mcp_registry.effective_servers().await,
    };
    let store = SessionStore::for_workspace(&options.workspace_root, &session_name)?;
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
            model: &resolved_model.invocation,
            subagent_identities: &subagent_identities,
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
        broadcast_message(
            &self.tx,
            ServerMessage::AgentEvent(Box::new(envelope.clone())),
        );
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
    let store = session_store(state, session_name)?;
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

fn session_store(state: &AppState, name: &str) -> Result<SessionStore, ApiError> {
    SessionStore::for_workspace(&state.inner.options.workspace_root, name)
        .map_err(|error| ApiError::bad_request(error.to_string()))
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

fn subagent_registry_error(error: SubagentRegistryError) -> ApiError {
    match error {
        SubagentRegistryError::Validation(_) => ApiError::bad_request(error.to_string()),
        SubagentRegistryError::Conflict(_) => ApiError::conflict(error.to_string()),
        SubagentRegistryError::NotFound(_) => ApiError::not_found(error.to_string()),
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
    use agent_protocol::{
        ModelInvocation, PermissionMode, ReasoningLevel, ReasoningProfile, ShellPolicy,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;
    use tower::ServiceExt;

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
        let subagent_registry =
            SubagentRegistry::load(options.subagent_store_path.clone()).expect("subagent registry");
        AppState {
            inner: Arc::new(ServerState {
                options,
                model_registry,
                mcp_registry,
                command_registry,
                subagent_registry,
                sessions: Mutex::new(HashMap::new()),
                mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
                access_policy: ServerAccessPolicy::Browser,
                shutting_down: AtomicBool::new(false),
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
        assert!(
            value["subagent_store_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("subagents.json"))
        );
        assert!(!value.to_string().contains("secret-test-key"));
    }

    #[tokio::test]
    async fn embedded_subagent_settings_routes_manage_the_global_profile_list() {
        let server = EmbeddedServer::new(test_options()).expect("embedded server");

        let settings = server
            .request("GET", "/api/subagent-settings", None)
            .await
            .expect("read subagent settings");
        assert_eq!(settings.status, 200);
        let settings = settings.body.expect("settings body");
        assert_eq!(settings["profiles"].as_array().map(Vec::len), Some(22));
        assert_eq!(settings["profiles"][0]["id"], "builtin-01");

        let created = server
            .request(
                "POST",
                "/api/subagents",
                Some(json!({"name": "测试成员", "avatar_data_url": null})),
            )
            .await
            .expect("create subagent");
        assert_eq!(created.status, 200);
        let id = created.body.expect("created body")["id"]
            .as_str()
            .expect("created id")
            .to_string();

        let updated = server
            .request(
                "PUT",
                &format!("/api/subagents/{id}"),
                Some(json!({"name": "更新成员", "avatar_data_url": null})),
            )
            .await
            .expect("update subagent");
        assert_eq!(updated.status, 200);
        assert_eq!(updated.body.expect("updated body")["name"], "更新成员");

        let duplicate = server
            .request(
                "POST",
                "/api/subagents",
                Some(json!({"name": "后藤一里", "avatar_data_url": null})),
            )
            .await
            .expect("duplicate response");
        assert_eq!(duplicate.status, 409);

        let deleted = server
            .request("DELETE", &format!("/api/subagents/{id}"), None)
            .await
            .expect("delete subagent");
        assert_eq!(deleted.status, 204);

        let reset = server
            .request("POST", "/api/subagent-settings/reset", None)
            .await
            .expect("reset subagents");
        assert_eq!(reset.status, 200);
        assert_eq!(
            reset.body.expect("reset body")["profiles"]
                .as_array()
                .map(Vec::len),
            Some(22)
        );
    }

    #[test]
    fn router_registers_model_routes_without_conflicts() {
        let _ = router(test_options()).expect("router");
    }

    #[test]
    fn embedded_index_references_assets_present_at_the_tauri_root() {
        let html = include_str!("../assets/index.html");

        assert!(html.contains(r#"src="/app.js""#));
        assert!(html.contains(r#"href="/style.css""#));
    }

    #[tokio::test]
    async fn browser_router_serves_root_and_legacy_asset_paths() {
        let router = router(test_options()).expect("browser router");
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
                        .expect("asset request"),
                )
                .await
                .expect("asset response");

            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static(content_type)),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn embedded_settings_prepare_ephemeral_remote_turn_runtime() {
        let server = EmbeddedServer::new(test_options()).expect("embedded server");
        let provider = server
            .request(
                "POST",
                "/api/model-providers",
                Some(serde_json::json!({
                    "name": "Managed",
                    "base_url": "https://models.example/v1",
                    "api_key": "managed-model-secret",
                    "enabled": true,
                    "timeout_secs": 30,
                    "models": [{
                        "id": "managed-model",
                        "name": "Managed model",
                        "context_window_tokens": 32_000,
                        "reserved_output_tokens": 4_000,
                        "supports_tools": true,
                        "reasoning_profile": "none"
                    }]
                })),
            )
            .await
            .expect("create provider");
        assert_eq!(provider.status, 200);
        let provider_id = provider.body.expect("provider body")["id"]
            .as_str()
            .expect("provider id")
            .to_string();
        let mcp = server
            .request(
                "POST",
                "/api/mcp-servers",
                Some(serde_json::json!({
                    "name": "managed-mcp",
                    "transport": "stdio",
                    "command": "managed-mcp",
                    "args": [],
                    "env": {"TOKEN": "managed-mcp-secret"},
                    "enabled": true,
                    "startup_timeout_sec": 10,
                    "tool_timeout_sec": 60
                })),
            )
            .await
            .expect("create MCP server");
        assert_eq!(mcp.status, 200);

        let turn = server
            .prepare_remote_turn(
                "default",
                serde_json::json!({
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
        assert_eq!(turn.managed_mcp_servers.len(), 1);
        assert_eq!(turn.subagent_identities.len(), 22);
        assert_eq!(turn.subagent_identities[0].id, "builtin-01");
        assert_eq!(
            turn.managed_mcp_servers[0]
                .env
                .get("TOKEN")
                .map(String::as_str),
            Some("managed-mcp-secret")
        );
    }

    #[tokio::test]
    async fn workspace_accepts_managed_model_resolved_by_desktop() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind model listener");
        let mut options = test_options();
        options.fallback_model = None;
        let server = EmbeddedServer::new_workspace(options).expect("workspace server");
        let mut subscription = server
            .subscribe_session("remote")
            .await
            .expect("subscribe session");

        server
            .start_remote_turn(RemoteTurnSpec {
                session: "remote".to_string(),
                request_id: "request-remote-model".to_string(),
                prompt: "hello".to_string(),
                permission_mode: Some(PermissionMode::WorkspaceWrite),
                model: RemoteTurnModel::Managed(RemoteModelSpec {
                    base_url: format!(
                        "http://{}/v1",
                        listener.local_addr().expect("model address")
                    ),
                    model: "deepseek-v4-pro".to_string(),
                    api_key: "remote-model-secret".to_string(),
                    timeout_secs: 30,
                    context_window_tokens: 65_536,
                    reserved_output_tokens: 8_192,
                    reasoning_profile: ReasoningProfile::Deepseek,
                    supports_tools: true,
                    invocation: ModelInvocation {
                        provider_id: "opencode".to_string(),
                        provider_name: "opencode".to_string(),
                        model_id: "deepseek-v4-pro".to_string(),
                        model_name: "DeepSeek V4 Pro".to_string(),
                        reasoning: ReasoningLevel::High,
                    },
                }),
                managed_mcp_servers: Vec::new(),
                subagent_identities: agent_protocol::default_subagent_identities(),
            })
            .await
            .expect("start remote turn");

        let accepted = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let message = subscription.recv().await?;
                match message["type"].as_str() {
                    Some("turn_rejected") => {
                        return Err(message["data"]["reason"]
                            .as_str()
                            .unwrap_or("turn rejected")
                            .to_string());
                    }
                    Some("snapshot") if !message["data"]["running_turn"].is_null() => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("remote turn acceptance event");

        assert!(accepted.is_ok(), "remote turn was rejected: {accepted:?}");
        server.shutdown(true).await;
    }

    #[tokio::test]
    async fn workspace_embedded_server_keeps_managed_settings_in_memory() {
        let options = test_options();
        let model_store = options.model_store_path.clone();
        let mcp_store = options.mcp_store_path.clone();
        let server = EmbeddedServer::new_workspace(options).expect("workspace server");

        let response = server
            .request("GET", "/api/model-settings", None)
            .await
            .expect("embedded response");

        assert_eq!(response.status, 404);
        assert!(!model_store.exists());
        assert!(!mcp_store.exists());
    }

    #[tokio::test]
    async fn desktop_access_bootstraps_cookie_and_rejects_unauthorized_requests() {
        let mut options = test_options();
        options.port = 43123;
        let (router, _) = build_router(options, ServerAccessPolicy::desktop("desktop-test-token"))
            .expect("desktop router");
        let host = "127.0.0.1:43123";

        let unauthorized = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let wrong_host = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::HOST, "localhost:43123")
                    .header(header::COOKIE, "morrow_desktop_session=desktop-test-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(wrong_host.status(), StatusCode::UNAUTHORIZED);

        let bootstrap = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/?desktop_bootstrap=desktop-test-token")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(bootstrap.status(), StatusCode::SEE_OTHER);
        let cookie = bootstrap
            .headers()
            .get(header::SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .expect("cookie text")
            .to_string();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));

        let authorized = router
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::HOST, host)
                    .header(header::COOKIE, "morrow_desktop_session=desktop-test-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(authorized.status(), StatusCode::OK);
        assert!(
            authorized
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
    }

    #[tokio::test]
    async fn browser_access_remains_available_without_a_desktop_token() {
        let response = router(test_options())
            .expect("browser router")
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn desktop_access_rejects_cross_origin_mutations_and_websockets() {
        let mut options = test_options();
        options.port = 43124;
        let (router, _) =
            build_router(options, ServerAccessPolicy::desktop("token")).expect("desktop router");
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/commands/resolve")
            .header(header::HOST, "127.0.0.1:43124")
            .header(header::ORIGIN, "https://example.com")
            .header(header::COOKIE, "morrow_desktop_session=token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"input":"hello"}"#))
            .expect("request");

        let response = router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let websocket = Request::builder()
            .uri("/api/sessions/default/ws")
            .header(header::HOST, "127.0.0.1:43124")
            .header(header::ORIGIN, "https://example.com")
            .header(header::COOKIE, "morrow_desktop_session=token")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(websocket).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn spawned_local_server_reports_address_and_shuts_down() {
        let mut server = spawn_local(test_options(), ServerAccessPolicy::Browser)
            .await
            .expect("spawn server");

        assert_ne!(server.addr().port(), 0);
        assert!(server.base_url().starts_with("http://127.0.0.1:"));
        assert!(server.activity().await.is_idle());
        server
            .shutdown(ShutdownPolicy::RequireIdle)
            .await
            .expect("shutdown server");
    }

    #[tokio::test]
    async fn require_idle_rejection_keeps_the_server_available() {
        let mut server = spawn_local(test_options(), ServerAccessPolicy::Browser)
            .await
            .expect("spawn server");
        let worker = tokio::spawn(std::future::pending::<()>());
        {
            let mut sessions = server.state.inner.sessions.lock().await;
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

        let result = server.shutdown(ShutdownPolicy::RequireIdle).await;

        assert!(matches!(result, Err(ServerError::RunningTurns(1))));
        assert_eq!(server.activity().await.running_turns, 1);
        server
            .shutdown(ShutdownPolicy::CancelRunning {
                timeout: Duration::from_millis(10),
            })
            .await
            .expect("cancel and shutdown server");
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
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let state = test_state();
        let workspace = state.inner.options.workspace_root.clone();
        let response = create_session(State(state), Path("fresh".to_string()))
            .await
            .expect("create session");
        let store = SessionStore::for_workspace(&workspace, "fresh").expect("store");
        let session = store.load_existing().expect("load created session");

        assert_eq!(response.0.session, Session::new());
        assert_eq!(session, Session::new());
        assert!(store.path().is_file());

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
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let state = test_state();
        let store = SessionStore::for_workspace(&state.inner.options.workspace_root, "existing")
            .expect("store");
        store.save(&Session::new()).expect("save existing session");

        let result = create_session(State(state), Path("existing".to_string())).await;

        assert!(matches!(
            result,
            Err(ApiError {
                status: StatusCode::CONFLICT,
                ..
            })
        ));

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
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let state = test_state();
        let store = SessionStore::for_workspace(&state.inner.options.workspace_root, "work")
            .expect("store");
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
