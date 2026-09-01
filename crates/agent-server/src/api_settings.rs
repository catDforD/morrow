use super::*;

pub(crate) async fn model_settings(State(state): State<AppState>) -> Json<ModelSettingsResponse> {
    Json(state.inner.model_registry.settings().await)
}

pub(crate) async fn hook_settings(
    State(state): State<AppState>,
) -> Result<Json<HookSettings>, ApiError> {
    state
        .inner
        .hook_manager
        .settings()
        .map(Json)
        .map_err(hook_api_error)
}

pub(crate) async fn trust_project_hooks(
    State(state): State<AppState>,
) -> Result<Json<HookSettings>, ApiError> {
    state
        .inner
        .hook_manager
        .trust_project()
        .map(Json)
        .map_err(hook_api_error)
}

pub(crate) async fn revoke_project_hooks(
    State(state): State<AppState>,
) -> Result<Json<HookSettings>, ApiError> {
    state
        .inner
        .hook_manager
        .revoke_project()
        .map(Json)
        .map_err(hook_api_error)
}

fn hook_api_error(error: agent_hooks::HookError) -> ApiError {
    match error {
        agent_hooks::HookError::ProjectConfigNotFound => ApiError::not_found(error.to_string()),
        agent_hooks::HookError::InvalidConfig { .. }
        | agent_hooks::HookError::ConfigParse { .. }
        | agent_hooks::HookError::UnsupportedConfigSchema { .. } => {
            ApiError::bad_request(error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}

pub(crate) async fn create_model_provider(
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

pub(crate) async fn update_model_provider(
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

pub(crate) async fn delete_model_provider(
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

pub(crate) async fn discover_model_provider(
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

pub(crate) async fn set_default_model(
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

pub(crate) async fn mcp_settings(State(state): State<AppState>) -> Json<McpSettingsResponse> {
    Json(state.inner.mcp_registry.settings().await)
}

pub(crate) async fn create_mcp_server(
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

pub(crate) async fn update_mcp_server(
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

pub(crate) async fn delete_mcp_server(
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

pub(crate) async fn import_mcp_servers(
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

pub(crate) async fn test_mcp_server(
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

pub(crate) async fn command_settings(
    State(state): State<AppState>,
) -> Result<Json<CommandSettingsResponse>, ApiError> {
    state
        .inner
        .command_registry
        .settings()
        .map(Json)
        .map_err(command_registry_error)
}

pub(crate) async fn subagent_settings(
    State(state): State<AppState>,
) -> Json<SubagentSettingsResponse> {
    Json(state.inner.subagent_registry.settings().await)
}

pub(crate) async fn update_subagent_role(
    State(state): State<AppState>,
    Path(role): Path<agent_protocol::SubagentRole>,
    Json(request): Json<SubagentRoleWriteRequest>,
) -> Result<Json<SubagentRoleSettingsResponse>, ApiError> {
    state
        .inner
        .subagent_registry
        .update_role(role, request)
        .await
        .map(Json)
        .map_err(subagent_registry_error)
}

pub(crate) async fn reset_subagent_roles(
    State(state): State<AppState>,
) -> Result<Json<Vec<SubagentRoleSettingsResponse>>, ApiError> {
    state
        .inner
        .subagent_registry
        .reset_roles()
        .await
        .map(Json)
        .map_err(subagent_registry_error)
}

pub(crate) async fn create_subagent(
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

pub(crate) async fn update_subagent(
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

pub(crate) async fn delete_subagent(
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

pub(crate) async fn reset_subagents(
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

pub(crate) async fn create_command(
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

pub(crate) async fn update_command(
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

pub(crate) async fn delete_command(
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

pub(crate) async fn resolve_command(
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
