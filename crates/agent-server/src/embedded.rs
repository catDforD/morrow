use super::*;

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
    ) -> Result<Option<SessionStreamFrame>, String> {
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
        let subagent_roles = self
            .remote_subagent_role_specs(session_name, &model)
            .await?;
        Ok(RemoteTurnSpec {
            session: session_name.to_string(),
            request_id,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
            subagent_roles,
        })
    }

    pub async fn prepare_remote_subagent_message(
        &self,
        session_name: &str,
        message: serde_json::Value,
    ) -> Result<RemoteSubagentMessageSpec, String> {
        let resume_selection = match serde_json::from_value::<ClientMessage>(message.clone())
            .map_err(|error| format!("invalid subagent session message: {error}"))?
        {
            ClientMessage::SendSubagent {
                model_selection, ..
            } => model_selection,
            ClientMessage::SpawnSubagent { .. } => None,
            _ => {
                return Err(
                    "remote subagent message must create or continue an instance".to_string(),
                );
            }
        };
        let inherited_model = self
            .service
            .inner
            .model_registry
            .resolve_remote_for_turn(session_name, None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(RemoteSubagentMessageSpec {
            session: session_name.to_string(),
            message,
            permission_mode: Some(self.service.inner.options.permissions.mode),
            subagent_identities: self.service.inner.subagent_registry.identities().await,
            subagent_roles: self
                .remote_subagent_role_specs(session_name, &inherited_model)
                .await?,
            resume_model: match resume_selection {
                Some(selection) => Some(
                    self.service
                        .inner
                        .model_registry
                        .resolve_remote_for_turn(session_name, Some(selection))
                        .await
                        .map_err(|error| error.to_string())?,
                ),
                None => None,
            },
        })
    }

    async fn remote_subagent_role_specs(
        &self,
        session_name: &str,
        inherited_model: &RemoteTurnModel,
    ) -> Result<Vec<RemoteSubagentRoleSpec>, String> {
        let overrides = self.service.inner.subagent_registry.role_overrides().await;
        let mut roles = Vec::with_capacity(SubagentRole::ALL.len());
        for role in SubagentRole::ALL {
            let role_override = overrides.get(&role).cloned().unwrap_or_default();
            let model = match role_override.model_selection.clone() {
                Some(selection) => self
                    .service
                    .inner
                    .model_registry
                    .resolve_remote_for_turn(session_name, Some(selection))
                    .await
                    .map_err(|error| {
                        format!("{} subagent model is unavailable: {error}", role.as_str())
                    })?,
                None => inherited_model.clone(),
            };
            roles.push(RemoteSubagentRoleSpec {
                role,
                overrides: role_override,
                model,
            });
        }
        Ok(roles)
    }

    pub async fn start_remote_turn(&self, turn: RemoteTurnSpec) -> Result<(), String> {
        let RemoteTurnSpec {
            session,
            request_id: _,
            prompt,
            permission_mode,
            model,
            managed_mcp_servers,
            subagent_identities,
            subagent_roles,
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
        let (subagent_role_overrides, subagent_role_models) = self
            .resolve_remote_subagent_roles(&session, subagent_roles)
            .await?;
        start_turn(
            self.service.clone(),
            session,
            StartTurnRequest {
                prompt,
                prompt_resolved: true,
                permission_mode,
                model_selection: None,
                resolved_model: Some(resolved_model),
                mcp_servers: Some(mcp_servers),
                subagent_identities: Some(subagent_identities),
                subagent_role_overrides: Some(subagent_role_overrides),
                subagent_role_models: Some(subagent_role_models),
            },
            tx,
        )
        .await
        .map(|_| ())
    }

    pub async fn send_remote_subagent_message(
        &self,
        command: RemoteSubagentMessageSpec,
    ) -> Result<(), String> {
        let session = command.session.clone();
        with_session_command(
            &self.service,
            &session,
            self.send_remote_subagent_message_inner(command),
        )
        .await
    }

    async fn send_remote_subagent_message_inner(
        &self,
        command: RemoteSubagentMessageSpec,
    ) -> Result<(), String> {
        let RemoteSubagentMessageSpec {
            session,
            message,
            permission_mode,
            subagent_identities,
            subagent_roles,
            resume_model,
        } = command;
        let (overrides, models) = self
            .resolve_remote_subagent_roles(&session, subagent_roles)
            .await?;
        let parent_model = models
            .get(&SubagentRole::Explore)
            .cloned()
            .or_else(|| models.values().next().cloned())
            .ok_or_else(|| "remote subagent runtime contains no models".to_string())?;
        let permissions = requested_permissions(
            self.service.inner.options.permissions,
            permission_mode,
            self.service.inner.options.permission_ceiling,
        );
        let middleware = self
            .service
            .inner
            .hook_manager
            .load_snapshot()
            .map_err(|error| error.to_string())?
            .registry();
        let supervisor = prepare_subagent_supervisor_with_runtime(SubagentSupervisorPreparation {
            state: &self.service,
            session_name: &session,
            parent_model: &parent_model,
            parent_permissions: permissions,
            identities: &subagent_identities,
            overrides,
            supplied_models: Some(models),
            middleware,
        })
        .await?;
        if let Some(model) = resume_model {
            let resolved = match model {
                RemoteTurnModel::WorkspaceFallback { selection } => self
                    .service
                    .inner
                    .model_registry
                    .resolve_for_turn(&session, Some(selection))
                    .await
                    .map_err(|error| error.to_string())?,
                RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
            };
            let client: Arc<dyn Model> = Arc::new(resolved.client.clone());
            supervisor
                .register_model_runtime(client, resolved.invocation, resolved.limits)
                .await;
        }
        let parsed = serde_json::from_value::<ClientMessage>(message)
            .map_err(|error| format!("invalid subagent session message: {error}"))?;
        let tx = session_sender(&self.service, &session).await;
        match parsed {
            ClientMessage::SpawnSubagent {
                request_id,
                role,
                task,
            } => {
                if let Err(reason) = supervisor.spawn(role, task).await {
                    broadcast_message(&tx, ServerMessage::SubagentRejected { request_id, reason });
                }
            }
            ClientMessage::SendSubagent {
                request_id,
                instance_id,
                message,
                ..
            } => {
                if let Err(reason) = supervisor.send(instance_id, message).await {
                    broadcast_message(&tx, ServerMessage::SubagentRejected { request_id, reason });
                }
            }
            _ => {
                return Err(
                    "remote subagent message must create or continue an instance".to_string(),
                );
            }
        }
        Ok(())
    }

    async fn resolve_remote_subagent_roles(
        &self,
        session: &str,
        roles: Vec<RemoteSubagentRoleSpec>,
    ) -> Result<
        (
            BTreeMap<SubagentRole, SubagentRoleOverride>,
            BTreeMap<SubagentRole, ResolvedModel>,
        ),
        String,
    > {
        if roles.len() != SubagentRole::ALL.len() {
            return Err("remote subagent runtime must contain all four roles".to_string());
        }
        let mut overrides = BTreeMap::new();
        let mut models = BTreeMap::new();
        for role in roles {
            if overrides.insert(role.role, role.overrides).is_some() {
                return Err(format!(
                    "duplicate remote subagent role {}",
                    role.role.as_str()
                ));
            }
            let model = match role.model {
                RemoteTurnModel::WorkspaceFallback { selection } => self
                    .service
                    .inner
                    .model_registry
                    .resolve_for_turn(session, Some(selection))
                    .await
                    .map_err(|error| error.to_string())?,
                RemoteTurnModel::Managed(spec) => resolved_model_from_remote(spec)?,
            };
            models.insert(role.role, model);
        }
        if SubagentRole::ALL
            .into_iter()
            .any(|role| !overrides.contains_key(&role))
        {
            return Err("remote subagent runtime is missing a built-in role".to_string());
        }
        Ok((overrides, models))
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

pub(crate) async fn index() -> Response {
    no_store(Html(include_str!("../assets/index.html")).into_response())
}

pub(crate) async fn app_js() -> Response {
    asset_response("app.js")
}

pub(crate) async fn style_css() -> Response {
    asset_response("style.css")
}

pub(crate) async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&path)
}

fn asset_response(path: &str) -> Response {
    let response = match path {
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
    };
    no_store(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
