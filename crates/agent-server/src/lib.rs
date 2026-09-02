mod commands;
mod mcp_settings;
mod models;
mod secrets;
mod subagent_settings;

pub use models::{FallbackModel, discover_models as discover_remote_models};
pub use subagent_settings::{
    SubagentProfileResponse, SubagentProfileWriteRequest, SubagentRegistryError,
    SubagentRoleSettingsResponse, SubagentRoleWriteRequest, SubagentSettingsResponse,
    load_subagent_identities,
};

use agent_config::{ContextConfig, LoadedServerConfig, McpServerConfig, ToolsConfig};
use agent_hooks::{HookManager, HookSettings};
use agent_model::{DEFAULT_MAX_RETRIES, ModelError, OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalOrigin, ApprovalRequest,
    ModelSelection, PermissionMode, PermissionProfile, ReasoningProfile, RemoteMcpServerSpec,
    RemoteModelConnectionSpec, RemoteModelSpec, RemoteSubagentMessageSpec, RemoteSubagentRoleSpec,
    RemoteTurnModel, RemoteTurnSpec, Session, SessionProjection, SessionStreamFrame, ShellPolicy,
    SubagentIdentity, SubagentInstanceSnapshot, SubagentRole, SubagentRoleOverride,
    SubagentRunRecord, WorkspaceLocation,
};
use agent_runtime::{
    AgentEventEnvelope, CancellationToken, McpInspection, McpToolCache, Model, RunAgentTurnContext,
    SessionHandle, SessionListingDiagnostic, SessionListingEntry, SessionStore,
    SessionSubscription, SubagentController, SubagentInstanceDocument, SubagentObserver,
    SubagentRoleRuntime, SubagentSupervisor, TurnEventHandler, WorkspaceInstructionsCache,
    inspect_mcp_servers, subagent_store_for_session,
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
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use subagent_settings::SubagentRegistry;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tower::ServiceExt;

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

pub async fn serve(
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
) -> Result<(), ServerError> {
    serve_with_ready(options, access_policy, |_| {}).await
}

pub async fn serve_with_ready(
    mut options: ServerOptions,
    access_policy: ServerAccessPolicy,
    on_ready: impl FnOnce(SocketAddr),
) -> Result<(), ServerError> {
    let addr = SocketAddr::new(options.host, options.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    let bound_addr = listener
        .local_addr()
        .map_err(|source| ServerError::Bind { addr, source })?;
    options.host = bound_addr.ip();
    options.port = bound_addr.port();
    let router = build_router(options, access_policy)?.0;
    on_ready(bound_addr);
    axum::serve(listener, router)
        .await
        .map_err(ServerError::Serve)
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

mod api_sessions;
pub(crate) use api_sessions::*;
mod api_settings;
pub(crate) use api_settings::*;
mod embedded;
pub use embedded::*;
mod error;
pub use error::*;
mod router;
pub use router::*;
mod state;
pub use state::*;
mod turns;
pub(crate) use turns::*;
mod ws;
pub use ws::*;
#[cfg(test)]
mod tests;
