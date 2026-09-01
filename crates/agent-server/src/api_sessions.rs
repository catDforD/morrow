use super::*;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusResponse {
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
pub(crate) struct SessionEntryResponse {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) turns: usize,
    pub(crate) active_messages: usize,
    pub(crate) summarized_turns: usize,
    pub(crate) has_summary: bool,
    pub(crate) archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionDirectoryDiagnosticResponse {
    pub(crate) name: Option<String>,
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionDirectoryResponse {
    pub(crate) schema_version: u32,
    pub(crate) sessions: Vec<SessionEntryResponse>,
    pub(crate) diagnostics: Vec<SessionDirectoryDiagnosticResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateSessionRequest {
    pub(crate) name: String,
}

pub(crate) async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
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

pub(crate) async fn get_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    require_active_session(&state, &name)?;
    Ok(Json(
        state.inner.model_registry.session_selection(&name).await,
    ))
}

pub(crate) async fn set_session_model_selection(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(selection): Json<ModelSelection>,
) -> Result<Json<SessionModelSelectionResponse>, ApiError> {
    require_active_session(&state, &name)?;
    state
        .inner
        .model_registry
        .set_session_selection(&name, selection)
        .await
        .map(Json)
        .map_err(model_registry_error)
}

pub(crate) async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<SessionDirectoryResponse>, ApiError> {
    session_directory_response(&state).map(Json)
}

pub(crate) async fn export_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let store = require_active_session(&state, &name)?;
    let handle = state
        .inner
        .sessions
        .lock()
        .await
        .get(&name)
        .and_then(|runtime| runtime.handle.clone());
    let bytes = match handle {
        Some(handle) => handle.export_document_bytes().await,
        None => store.export_document_bytes(),
    }
    .map_err(|error| ApiError::internal(error.to_string()))?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"session.jsonl\""),
    );
    Ok(response)
}

pub(crate) async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionEntryResponse>), ApiError> {
    let name = request.name;
    if session_has_active_work(&state, &name).await {
        return Err(ApiError::conflict("session has active agent work"));
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

    store
        .reset()
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let entry = session_entry_for_name(&state, &name, false)?;
    Ok((StatusCode::CREATED, Json(entry)))
}

pub(crate) async fn reset_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionEntryResponse>, ApiError> {
    let store = require_active_session(&state, &name)?;
    let handle = begin_session_lifecycle(&state, &name).await?;
    let result = async {
        match handle {
            Some(handle) => handle
                .hard_reset()
                .await
                .map_err(|error| ApiError::internal(error.to_string()))?,
            None => store
                .reset()
                .map_err(|error| ApiError::internal(error.to_string()))?,
        };
        let subagents = subagent_store_for_session(&state.inner.options.workspace_root, &name)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        subagents
            .reset()
            .map_err(|error| ApiError::internal(error.to_string()))?;
        if let Some(runtime) = state.inner.sessions.lock().await.get_mut(&name) {
            runtime.supervisor = None;
            runtime.approvals.clear();
        }
        session_entry_for_name(&state, &name, false).map(Json)
    }
    .await;
    finish_session_lifecycle(&state, &name).await;
    result
}

pub(crate) async fn archive_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionEntryResponse>, ApiError> {
    let store = require_active_session(&state, &name)?;
    let handle = begin_session_lifecycle(&state, &name).await?;
    let result = async {
        let subagents = subagent_store_for_session(&state.inner.options.workspace_root, &name)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        subagents
            .archive()
            .map_err(|error| ApiError::internal(error.to_string()))?;
        let archived = match handle {
            Some(handle) => handle.archive().await,
            None => store.archive(),
        };
        if let Err(error) = archived.map_err(session_mutation_error) {
            let _ = subagents.restore();
            return Err(error);
        }
        session_entry_for_name(&state, &name, true).map(Json)
    }
    .await;
    if result.is_ok() {
        state.inner.sessions.lock().await.remove(&name);
    } else {
        finish_session_lifecycle(&state, &name).await;
    }
    result
}

pub(crate) async fn restore_session(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SessionEntryResponse>, ApiError> {
    let _handle = begin_session_lifecycle(&state, &name).await?;
    let result = async {
        let store = session_store(&state, &name)?;
        store.restore().map_err(session_mutation_error)?;
        let subagents = subagent_store_for_session(&state.inner.options.workspace_root, &name)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        if let Err(error) = subagents.restore() {
            let _ = store.archive();
            return Err(ApiError::internal(error.to_string()));
        }
        session_entry_for_name(&state, &name, false).map(Json)
    }
    .await;
    state.inner.sessions.lock().await.remove(&name);
    result
}

fn session_directory_response(state: &AppState) -> Result<SessionDirectoryResponse, ApiError> {
    let store = session_store(state, &state.inner.options.default_session_name)?;
    let listing = store
        .list_current_scope_with_diagnostics()
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(SessionDirectoryResponse {
        schema_version: SESSION_DIRECTORY_SCHEMA_VERSION,
        sessions: listing
            .entries
            .into_iter()
            .map(session_entry_response)
            .collect(),
        diagnostics: listing
            .diagnostics
            .into_iter()
            .map(session_directory_diagnostic_response)
            .collect(),
    })
}

fn session_entry_for_name(
    state: &AppState,
    name: &str,
    archived: bool,
) -> Result<SessionEntryResponse, ApiError> {
    session_directory_response(state)?
        .sessions
        .into_iter()
        .find(|entry| entry.name == name && entry.archived == archived)
        .ok_or_else(|| ApiError::internal(format!("session {name:?} disappeared from directory")))
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

fn session_directory_diagnostic_response(
    diagnostic: SessionListingDiagnostic,
) -> SessionDirectoryDiagnosticResponse {
    SessionDirectoryDiagnosticResponse {
        name: diagnostic.name,
        path: diagnostic.path.display().to_string(),
        message: diagnostic.message,
    }
}
