use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_app::{
    CommandWriteRequest, DefaultModelRequest, DiscoverModelsRequest, DiscoveredModel,
    ManagedMcpTransport, ManagedModel, McpServerResponse, McpServerTestRequest,
    McpServerWriteRequest, ModelProviderResponse, ProviderWriteRequest,
    SessionCommand as AppSessionCommand, SessionEntry, SubagentProfileWriteRequest,
    SubagentRoleWriteRequest, WorkspaceApp, WorkspaceEvent as AppEvent,
};
use agent_protocol::{ModelSelection, PermissionProfile, ReasoningProfile};
use agent_tui::{
    BackendCommand, BackendError, CommandResult, ContextEstimate, ManagedCommandDraft,
    ManagedCommandView, ManagedModelSpec, McpServerDraft, McpServerSource, McpServerView,
    McpTransport, ModelOption, ModelProviderDraft, ModelProviderView, SecretValue, SessionInfo,
    SessionSnapshot, SettingsCommand, SettingsSnapshot, SubagentIdentityDraft,
    SubagentIdentityView, SubagentRoleView, SubagentTranscript, WorkspaceBackend, WorkspaceEvent,
    WorkspaceSnapshot,
};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::sync::{Mutex, mpsc};

pub struct LocalWorkspaceBackend {
    app: WorkspaceApp,
    workspace_root: PathBuf,
    event_tx: mpsc::UnboundedSender<WorkspaceEvent>,
    event_rx: Mutex<mpsc::UnboundedReceiver<WorkspaceEvent>>,
    subscriptions: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    request_sequence: AtomicU64,
}

impl LocalWorkspaceBackend {
    pub fn new(app: WorkspaceApp, workspace_root: PathBuf) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            app,
            workspace_root,
            event_tx,
            event_rx: Mutex::new(event_rx),
            subscriptions: Mutex::new(HashMap::new()),
            request_sequence: AtomicU64::new(1),
        }
    }

    fn request_id(&self, prefix: &str) -> String {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        format!("tui-{prefix}-{}-{sequence}", agent_runtime::timestamp_ms())
    }

    async fn ensure_subscription(&self, session_id: &str) -> Result<(), BackendError> {
        {
            let subscriptions = self.subscriptions.lock().await;
            if subscriptions.contains_key(session_id) {
                return Ok(());
            }
        }

        let mut subscription = match self.app.subscribe_session(session_id).await {
            Ok(subscription) => subscription,
            Err(error) => {
                return Err(backend_error(error));
            }
        };
        let event_tx = self.event_tx.clone();
        let subscribed_session = session_id.to_string();
        for event in translate_event(&subscribed_session, subscription.snapshot.clone()) {
            let _ = event_tx.send(event);
        }
        let task_session = subscribed_session.clone();
        let task = tokio::spawn(async move {
            loop {
                match subscription.recv().await {
                    Ok(event) => {
                        for event in translate_event(&task_session, event) {
                            if event_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                    Err(agent_app::SubscriptionError::Lagged(_)) => {
                        // Reconcile the affected session itself, including background sessions.
                        // A workspace-only refresh cannot restore that session's persisted history
                        // and authoritative running/approval state.
                        if event_tx
                            .send(WorkspaceEvent::TurnSaved {
                                session_id: task_session.clone(),
                            })
                            .is_err()
                        {
                            return;
                        }
                        if event_tx.send(WorkspaceEvent::BroadcastLagged).is_err() {
                            return;
                        }
                    }
                    Err(agent_app::SubscriptionError::Closed) => return,
                }
            }
        });
        let mut subscriptions = self.subscriptions.lock().await;
        if let Some(previous) = subscriptions.insert(subscribed_session, task.abort_handle()) {
            previous.abort();
        }
        Ok(())
    }

    async fn session_info(&self, entry: &SessionEntry) -> Result<SessionInfo, BackendError> {
        let model = self
            .app
            .session_model_selection(&entry.name)
            .await
            .map_err(backend_error)?
            .selection;
        let running = if entry.archived {
            false
        } else {
            let AppEvent::Snapshot {
                running_turn,
                subagents,
                approvals,
                ..
            } = self
                .app
                .session_snapshot(&entry.name)
                .await
                .map_err(backend_error)?
            else {
                return Err(BackendError::new("应用层返回了无效的会话快照"));
            };
            let _ = self.event_tx.send(WorkspaceEvent::ApprovalQueue {
                session_id: entry.name.clone(),
                approvals,
            });
            let _ = self.event_tx.send(WorkspaceEvent::SubagentsChanged {
                session_id: entry.name.clone(),
                subagents,
            });
            running_turn.is_some()
        };
        Ok(SessionInfo {
            id: entry.name.clone(),
            title: entry.name.clone(),
            archived: entry.archived,
            running,
            model,
            permissions: self.app.default_permissions(),
        })
    }

    async fn session_infos(&self) -> Result<Vec<SessionInfo>, BackendError> {
        let entries = self.app.list_sessions().await.map_err(backend_error)?;
        let mut sessions = Vec::with_capacity(entries.len());
        for entry in &entries {
            if !entry.archived {
                self.ensure_subscription(&entry.name).await?;
            }
            sessions.push(self.session_info(entry).await?);
        }
        Ok(sessions)
    }

    async fn snapshot_for_session(
        &self,
        session_id: &str,
    ) -> Result<SessionSnapshot, BackendError> {
        self.ensure_subscription(session_id).await?;
        let selection = self
            .app
            .session_model_selection(session_id)
            .await
            .map_err(backend_error)?
            .selection;
        let AppEvent::Snapshot {
            session,
            running_turn,
            permissions,
            subagents,
            approvals,
        } = self
            .app
            .session_snapshot(session_id)
            .await
            .map_err(backend_error)?
        else {
            return Err(BackendError::new("应用层返回了无效的会话快照"));
        };
        Ok(SessionSnapshot {
            info: SessionInfo {
                id: session_id.to_string(),
                title: session_id.to_string(),
                archived: false,
                running: running_turn.is_some(),
                model: selection,
                permissions,
            },
            session,
            subagents,
            approvals,
        })
    }

    async fn publish_sessions_changed(&self) -> Result<(), BackendError> {
        let sessions = self.session_infos().await?;
        let _ = self
            .event_tx
            .send(WorkspaceEvent::SessionsChanged(sessions));
        Ok(())
    }

    async fn publish_session_resources(&self, session_id: &str) -> Result<(), BackendError> {
        let AppEvent::Snapshot {
            subagents,
            approvals,
            ..
        } = self
            .app
            .session_snapshot(session_id)
            .await
            .map_err(backend_error)?
        else {
            return Err(BackendError::new("应用层返回了无效的会话快照"));
        };
        let _ = self.event_tx.send(WorkspaceEvent::ApprovalQueue {
            session_id: session_id.to_string(),
            approvals,
        });
        let _ = self.event_tx.send(WorkspaceEvent::SubagentsChanged {
            session_id: session_id.to_string(),
            subagents,
        });
        Ok(())
    }

    async fn create_unique_session(
        &self,
    ) -> Result<(String, agent_protocol::SessionDocument), BackendError> {
        let base = format!("session-{}", agent_runtime::timestamp_ms());
        let mut suffix = 0_u32;
        loop {
            let name = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            match self.app.create_session(&name).await {
                Ok(document) => return Ok((name, document)),
                Err(error) if error.kind() == agent_app::WorkspaceErrorKind::Conflict => {
                    suffix = suffix.saturating_add(1);
                }
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn settings_result(&self) -> Result<CommandResult, BackendError> {
        Ok(CommandResult::Settings(self.load_settings().await?))
    }

    async fn save_model_provider(
        &self,
        draft: ModelProviderDraft,
    ) -> Result<CommandResult, BackendError> {
        let settings = self.app.model_settings().await;
        let current = settings
            .providers
            .iter()
            .find(|provider| provider.id == draft.id);
        if draft.read_only || current.is_some_and(|provider| provider.read_only) {
            return Err(BackendError::new(
                "该模型供应商来自 morrow.toml，只读且无法保存",
            ));
        }
        if current.is_none() && draft.api_key.is_empty() {
            return Err(BackendError::new("新建模型供应商时 API key 不能为空"));
        }
        let mut request = provider_request_from_draft(&draft);
        if current.is_none() && request.models.is_empty() {
            let discovered = self
                .app
                .discover_models(DiscoverModelsRequest {
                    provider_id: None,
                    base_url: Some(draft.base_url.clone()),
                    api_key: secret_update(&draft.api_key),
                    timeout_secs: draft.timeout_secs,
                })
                .await
                .map_err(backend_error)?;
            request.models = merge_discovered_models(&[], discovered.models);
        }
        if current.is_none()
            && settings.default_selection.is_none()
            && request.default_model.is_none()
        {
            let first = request
                .models
                .first()
                .ok_or_else(|| BackendError::new("供应商没有返回可用模型"))?;
            request.default_model = Some(DefaultModelRequest {
                model_id: first.id.clone(),
                reasoning: agent_protocol::ReasoningLevel::Off,
            });
        }
        if current.is_some() {
            self.app
                .update_model_provider(&draft.id, request)
                .await
                .map_err(backend_error)?;
        } else {
            self.app
                .create_model_provider(request)
                .await
                .map_err(backend_error)?;
        }
        self.publish_sessions_changed().await?;
        self.settings_result().await
    }

    async fn update_model_api_key(
        &self,
        provider_id: String,
        api_key: SecretValue,
    ) -> Result<CommandResult, BackendError> {
        let settings = self.app.model_settings().await;
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| BackendError::new(format!("模型供应商 {provider_id:?} 不存在")))?;
        self.app
            .update_model_provider(
                &provider_id,
                ProviderWriteRequest {
                    name: provider.name.clone(),
                    base_url: provider.base_url.clone(),
                    api_key: secret_update(&api_key),
                    enabled: provider.enabled,
                    timeout_secs: provider.timeout_secs,
                    models: provider.models.clone(),
                    default_model: None,
                },
            )
            .await
            .map_err(backend_error)?;
        self.settings_result().await
    }

    async fn save_mcp_server(&self, draft: McpServerDraft) -> Result<CommandResult, BackendError> {
        if draft.read_only || draft.source == McpServerSource::RuntimeConfig {
            return Err(BackendError::new(
                "该 MCP 服务来自 morrow.toml，只读且无法保存",
            ));
        }
        let original_name = draft.original_name.clone();
        let request = mcp_request_from_draft(draft);
        if let Some(original_name) = original_name {
            self.app
                .update_mcp_server(&original_name, request)
                .await
                .map_err(backend_error)?;
        } else {
            self.app
                .create_mcp_server(request)
                .await
                .map_err(backend_error)?;
        }
        self.settings_result().await
    }

    async fn set_mcp_enabled(
        &self,
        name: String,
        enabled: bool,
    ) -> Result<CommandResult, BackendError> {
        let settings = self.app.mcp_settings().await;
        let server = settings
            .servers
            .iter()
            .find(|server| server.name == name)
            .ok_or_else(|| BackendError::new(format!("MCP 服务 {name:?} 不存在")))?;
        if server.read_only {
            return Err(BackendError::new(
                "该 MCP 服务来自 morrow.toml，只读且无法启停",
            ));
        }
        let mut request = mcp_request_from_response(server);
        request.enabled = enabled;
        self.app
            .update_mcp_server(&name, request)
            .await
            .map_err(backend_error)?;
        self.settings_result().await
    }

    async fn save_managed_command(
        &self,
        draft: ManagedCommandDraft,
    ) -> Result<CommandResult, BackendError> {
        let request = CommandWriteRequest {
            name: draft.name,
            description: draft.description,
            argument_hint: draft.argument_hint,
            prompt: draft.prompt,
        };
        if let Some(original_name) = draft.original_name {
            self.app
                .update_command(&original_name, request)
                .await
                .map_err(backend_error)?;
        } else {
            self.app
                .create_command(request)
                .await
                .map_err(backend_error)?;
        }
        self.settings_result().await
    }

    async fn save_subagent_identity(
        &self,
        draft: SubagentIdentityDraft,
    ) -> Result<CommandResult, BackendError> {
        let avatar_data_url = match draft.avatar_path {
            Some(path) => Some(read_avatar_data_url(&self.workspace_root, &path)?),
            None if draft.remove_avatar => None,
            None => match draft.original_id.as_deref() {
                Some(id) => self
                    .app
                    .subagent_settings()
                    .await
                    .profiles
                    .into_iter()
                    .find(|profile| profile.id == id)
                    .and_then(|profile| profile.avatar_data_url),
                None => None,
            },
        };
        let request = SubagentProfileWriteRequest {
            name: draft.identity.name,
            avatar_data_url,
        };
        if let Some(id) = draft.original_id {
            self.app
                .update_subagent(&id, request)
                .await
                .map_err(backend_error)?;
        } else {
            self.app
                .create_subagent(request)
                .await
                .map_err(backend_error)?;
        }
        self.settings_result().await
    }

    async fn execute_settings(
        &self,
        command: SettingsCommand,
    ) -> Result<CommandResult, BackendError> {
        match command {
            SettingsCommand::SaveModelProvider(draft) => self.save_model_provider(draft).await,
            SettingsCommand::DeleteModelProvider { provider_id } => {
                self.app
                    .delete_model_provider(&provider_id)
                    .await
                    .map_err(backend_error)?;
                self.publish_sessions_changed().await?;
                self.settings_result().await
            }
            SettingsCommand::DiscoverModels { provider_id } => {
                let settings = self.app.model_settings().await;
                let provider = settings
                    .providers
                    .iter()
                    .find(|provider| provider.id == provider_id)
                    .ok_or_else(|| {
                        BackendError::new(format!("模型供应商 {provider_id:?} 不存在"))
                    })?;
                let discovered = self
                    .app
                    .discover_models(DiscoverModelsRequest {
                        provider_id: Some(provider_id.clone()),
                        base_url: None,
                        api_key: None,
                        timeout_secs: provider.timeout_secs,
                    })
                    .await
                    .map_err(backend_error)?;
                if provider.read_only {
                    let names = discovered
                        .models
                        .into_iter()
                        .map(|model| model.id)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(CommandResult::Notice(format!("发现模型：{names}")));
                }
                let models = merge_discovered_models(&provider.models, discovered.models);
                self.app
                    .update_model_provider(
                        &provider_id,
                        ProviderWriteRequest {
                            name: provider.name.clone(),
                            base_url: provider.base_url.clone(),
                            api_key: None,
                            enabled: provider.enabled,
                            timeout_secs: provider.timeout_secs,
                            models,
                            default_model: None,
                        },
                    )
                    .await
                    .map_err(backend_error)?;
                self.publish_sessions_changed().await?;
                self.settings_result().await
            }
            SettingsCommand::SetDefaultModel(selection) => {
                self.app
                    .set_default_model(selection)
                    .await
                    .map_err(backend_error)?;
                self.publish_sessions_changed().await?;
                self.settings_result().await
            }
            SettingsCommand::UpdateModelApiKey {
                provider_id,
                api_key,
            } => self.update_model_api_key(provider_id, api_key).await,
            SettingsCommand::SaveMcpServer(draft) => self.save_mcp_server(draft).await,
            SettingsCommand::ImportMcpServers { source } => {
                let value = serde_json::from_str(&source)
                    .map_err(|error| BackendError::new(format!("MCP 导入 JSON 无效：{error}")))?;
                self.app
                    .import_mcp_servers(value)
                    .await
                    .map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::TestMcpServer { name } => {
                let settings = self.app.mcp_settings().await;
                let server = settings
                    .servers
                    .iter()
                    .find(|server| server.name == name)
                    .ok_or_else(|| BackendError::new(format!("MCP 服务 {name:?} 不存在")))?;
                if server.read_only {
                    return Err(BackendError::new(
                        "morrow.toml 中的 MCP 服务隐藏了敏感值，无法从设置页测试",
                    ));
                }
                let inspection = self
                    .app
                    .test_mcp_server(McpServerTestRequest {
                        existing_name: Some(name),
                        server: mcp_request_from_response(server),
                    })
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Notice(format!("{inspection:?}")))
            }
            SettingsCommand::TestMcpServerDraft(draft) => {
                if draft.read_only || draft.source == McpServerSource::RuntimeConfig {
                    return Err(BackendError::new(
                        "morrow.toml 中的 MCP 服务隐藏了敏感值，无法从设置页测试",
                    ));
                }
                let existing_name = draft.original_name.clone();
                let inspection = self
                    .app
                    .test_mcp_server(McpServerTestRequest {
                        existing_name,
                        server: mcp_request_from_draft(draft),
                    })
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Notice(format!("{inspection:?}")))
            }
            SettingsCommand::SetMcpEnabled { name, enabled } => {
                self.set_mcp_enabled(name, enabled).await
            }
            SettingsCommand::DeleteMcpServer { name } => {
                self.app
                    .delete_mcp_server(&name)
                    .await
                    .map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::SaveManagedCommand(draft) => self.save_managed_command(draft).await,
            SettingsCommand::DeleteManagedCommand { name } => {
                self.app
                    .delete_command(&name)
                    .await
                    .map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::SaveSubagentIdentity(draft) => {
                self.save_subagent_identity(draft).await
            }
            SettingsCommand::DeleteSubagentIdentity { id } => {
                self.app.delete_subagent(&id).await.map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::SaveSubagentRole(role) => {
                self.app
                    .update_subagent_role(
                        role.role,
                        SubagentRoleWriteRequest {
                            model_selection: role.settings.model_selection,
                            prompt_suffix: role.settings.prompt_suffix,
                            timeout_secs: role.settings.timeout_secs,
                            max_tool_rounds: role.settings.max_tool_rounds,
                        },
                    )
                    .await
                    .map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::ResetSubagentRoles => {
                self.app
                    .reset_subagent_roles()
                    .await
                    .map_err(backend_error)?;
                self.settings_result().await
            }
            SettingsCommand::ResetSubagentProfiles => {
                self.app.reset_subagents().await.map_err(backend_error)?;
                self.settings_result().await
            }
        }
    }
}

#[async_trait]
impl WorkspaceBackend for LocalWorkspaceBackend {
    async fn snapshot(
        &self,
        preferred_session: Option<&str>,
    ) -> Result<WorkspaceSnapshot, BackendError> {
        let mut sessions = self.session_infos().await?;
        let preferred = preferred_session
            .filter(|name| {
                !sessions
                    .iter()
                    .any(|session| session.id == *name && session.archived)
            })
            .map(str::to_string)
            .or_else(|| {
                sessions
                    .iter()
                    .find(|session| !session.archived)
                    .map(|session| session.id.clone())
            });
        let preferred = match preferred {
            Some(preferred) => preferred,
            None => {
                let default = self.app.options().default_session_name.clone();
                if sessions
                    .iter()
                    .any(|session| session.id == default && session.archived)
                {
                    self.create_unique_session().await?.0
                } else {
                    default
                }
            }
        };
        let active_session = self.snapshot_for_session(&preferred).await?;
        if !sessions.iter().any(|session| session.id == preferred) {
            sessions.push(active_session.info.clone());
        }
        let model_settings = self.app.model_settings().await;
        Ok(WorkspaceSnapshot {
            sessions,
            active_session: Some(active_session),
            models: model_options(&model_settings.providers),
        })
    }

    async fn recv_event(&self) -> Result<WorkspaceEvent, BackendError> {
        self.event_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| BackendError::new("工作区事件流已关闭"))
    }

    async fn execute(&self, command: BackendCommand) -> Result<CommandResult, BackendError> {
        match command {
            BackendCommand::CreateSession => {
                let (name, document) = self.create_unique_session().await?;
                self.ensure_subscription(&name).await?;
                let model = self
                    .app
                    .session_model_selection(&name)
                    .await
                    .ok()
                    .and_then(|selection| selection.selection);
                let snapshot = SessionSnapshot {
                    info: SessionInfo {
                        id: name.clone(),
                        title: name,
                        archived: false,
                        running: false,
                        model,
                        permissions: self.app.default_permissions(),
                    },
                    session: document.session,
                    subagents: Vec::new(),
                    approvals: Vec::new(),
                };
                self.publish_sessions_changed().await?;
                Ok(CommandResult::SessionCreated(snapshot))
            }
            BackendCommand::LoadSession { session_id } => Ok(CommandResult::Session(
                self.snapshot_for_session(&session_id).await?,
            )),
            BackendCommand::ResetSession { session_id } => {
                let document = self
                    .app
                    .reset_session(&session_id)
                    .await
                    .map_err(backend_error)?;
                let mut snapshot = self.snapshot_for_session(&session_id).await?;
                snapshot.session = document.session;
                Ok(CommandResult::Session(snapshot))
            }
            BackendCommand::SetSessionModel {
                session_id,
                selection,
            } => {
                self.app
                    .set_session_model_selection(&session_id, selection)
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Session(
                    self.snapshot_for_session(&session_id).await?,
                ))
            }
            BackendCommand::ArchiveSession { session_id } => {
                self.app
                    .archive_session(&session_id)
                    .await
                    .map_err(backend_error)?;
                if let Some(subscription) = self.subscriptions.lock().await.remove(&session_id) {
                    subscription.abort();
                }
                self.publish_sessions_changed().await?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::RestoreSession { session_id } => {
                self.app
                    .restore_session(&session_id)
                    .await
                    .map_err(backend_error)?;
                self.ensure_subscription(&session_id).await?;
                self.publish_sessions_changed().await?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::StartTurn {
                session_id,
                prompt,
                model,
                permissions,
            } => {
                self.ensure_subscription(&session_id).await?;
                self.app
                    .send_session_command(
                        &session_id,
                        AppSessionCommand::StartTurn {
                            request_id: self.request_id("turn"),
                            prompt,
                            prompt_resolved: false,
                            permissions,
                            model_selection: model,
                        },
                    )
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::CancelTurn { session_id } => {
                let turn_id = match self
                    .app
                    .session_snapshot(&session_id)
                    .await
                    .map_err(backend_error)?
                {
                    AppEvent::Snapshot {
                        running_turn: Some(turn),
                        ..
                    } => turn.turn_id,
                    _ => return Ok(CommandResult::Notice("该会话没有运行中的任务".to_string())),
                };
                self.app
                    .send_session_command(&session_id, AppSessionCommand::CancelTurn { turn_id })
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::ResolveApproval {
                session_id,
                decision,
            } => {
                self.app
                    .send_session_command(
                        &session_id,
                        AppSessionCommand::ApprovalDecision {
                            request_id: decision.request_id,
                            approved: decision.approved,
                        },
                    )
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::CompactSession { session_id } => {
                let outcome = self
                    .app
                    .compact_session(&session_id, None)
                    .await
                    .map_err(backend_error)?;
                let snapshot = self.snapshot_for_session(&session_id).await?;
                let _ = self
                    .event_tx
                    .send(WorkspaceEvent::SessionLoaded(snapshot.clone()));
                Ok(CommandResult::Notice(match outcome {
                    agent_runtime::CompactionOutcome::Changed => "会话已压缩".to_string(),
                    agent_runtime::CompactionOutcome::Noop => "没有可压缩的会话历史".to_string(),
                }))
            }
            BackendCommand::FollowUpSubagent {
                session_id,
                instance_id,
                prompt,
            } => {
                self.app
                    .send_session_command(
                        &session_id,
                        AppSessionCommand::SendSubagent {
                            request_id: self.request_id("subagent"),
                            instance_id,
                            message: prompt,
                            model_selection: None,
                        },
                    )
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::CancelSubagent {
                session_id,
                instance_id,
            } => {
                self.app
                    .send_session_command(
                        &session_id,
                        AppSessionCommand::CancelSubagent { instance_id },
                    )
                    .await
                    .map_err(backend_error)?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::DeleteSubagent {
                session_id,
                instance_id,
            } => {
                self.app
                    .send_session_command(
                        &session_id,
                        AppSessionCommand::DeleteSubagent { instance_id },
                    )
                    .await
                    .map_err(backend_error)?;
                self.publish_session_resources(&session_id).await?;
                Ok(CommandResult::Ack)
            }
            BackendCommand::LoadSubagentTranscript {
                session_id,
                instance_id,
            } => {
                let transcript = self
                    .app
                    .subagent_transcript(&session_id, &instance_id)
                    .await
                    .map_err(backend_error)?;
                let lines = transcript
                    .events
                    .iter()
                    .filter_map(|event| serde_json::to_string(&event.event).ok())
                    .collect();
                Ok(CommandResult::SubagentTranscript(SubagentTranscript {
                    instance: transcript.instance,
                    lines,
                }))
            }
            BackendCommand::Settings(command) => self.execute_settings(command).await,
        }
    }

    async fn load_settings(&self) -> Result<SettingsSnapshot, BackendError> {
        let model_settings = self.app.model_settings().await;
        let mcp_settings = self.app.mcp_settings().await;
        let command_settings = self.app.command_settings().map_err(backend_error)?;
        let subagent_settings = self.app.subagent_settings().await;

        let providers = model_settings
            .providers
            .iter()
            .map(model_provider_view)
            .collect();
        let models = model_options(&model_settings.providers);
        let mcp_servers = mcp_settings.servers.iter().map(mcp_server_view).collect();
        let commands = command_settings
            .commands
            .into_iter()
            .map(|command| ManagedCommandView {
                name: command.name,
                description: command.description,
                argument_hint: command.argument_hint,
                prompt: command.prompt,
            })
            .collect();
        let subagent_identities = subagent_settings
            .profiles
            .into_iter()
            .map(|profile| SubagentIdentityView {
                identity: agent_protocol::SubagentIdentity {
                    id: profile.id,
                    name: profile.name,
                },
                avatar_configured: profile.avatar_data_url.is_some(),
                avatar_path: None,
            })
            .collect();
        let subagent_roles = subagent_settings
            .roles
            .into_iter()
            .map(|role| SubagentRoleView {
                role: role.role,
                settings: role.overrides,
            })
            .collect();

        Ok(SettingsSnapshot {
            providers,
            models,
            default_model: model_settings.default_selection,
            mcp_servers,
            commands,
            subagent_identities,
            subagent_roles,
        })
    }

    async fn estimate_context(
        &self,
        session_id: &str,
        draft: &str,
        model: Option<ModelSelection>,
        permissions: PermissionProfile,
    ) -> Result<ContextEstimate, BackendError> {
        let estimate = self
            .app
            .estimate_context(session_id, draft, permissions, model)
            .await
            .map_err(backend_error)?;
        Ok(ContextEstimate {
            used_tokens: estimate.estimated_tokens,
            input_budget_tokens: estimate.input_limit_tokens,
            auto_compact_at_tokens: estimate.auto_compact_trigger_tokens,
        })
    }
}

fn translate_event(session_id: &str, event: AppEvent) -> Vec<WorkspaceEvent> {
    match event {
        AppEvent::Snapshot {
            running_turn,
            subagents,
            approvals,
            ..
        } => vec![
            WorkspaceEvent::SessionRunning {
                session_id: session_id.to_string(),
                running: running_turn.is_some(),
            },
            WorkspaceEvent::ApprovalQueue {
                session_id: session_id.to_string(),
                approvals,
            },
            WorkspaceEvent::SubagentsChanged {
                session_id: session_id.to_string(),
                subagents,
            },
        ],
        AppEvent::AgentEvent(envelope) => vec![WorkspaceEvent::Agent {
            session_id: envelope.session.clone(),
            origin: envelope.origin.clone(),
            event: envelope.event.clone(),
        }],
        AppEvent::TurnSaved { session, .. } => {
            vec![WorkspaceEvent::TurnSaved {
                session_id: session,
            }]
        }
        AppEvent::TurnRejected { reason, .. } => vec![
            WorkspaceEvent::Notice(reason),
            WorkspaceEvent::BroadcastLagged,
        ],
        AppEvent::SubagentRejected { reason, .. } => vec![WorkspaceEvent::Notice(reason)],
        AppEvent::ApprovalQueueUpdated { approvals } => vec![WorkspaceEvent::ApprovalQueue {
            session_id: session_id.to_string(),
            approvals,
        }],
        AppEvent::SubagentTranscript { .. } => Vec::new(),
        AppEvent::SubagentDeleted { instance_id } => vec![WorkspaceEvent::Notice(format!(
            "已删除 Subagent {instance_id}"
        ))],
        AppEvent::Error { message } => vec![
            WorkspaceEvent::Notice(message),
            WorkspaceEvent::BroadcastLagged,
        ],
    }
}

fn model_options(providers: &[agent_app::ModelProviderResponse]) -> Vec<ModelOption> {
    providers
        .iter()
        .filter(|provider| provider.enabled)
        .flat_map(|provider| {
            provider.models.iter().map(|model| ModelOption {
                provider_id: provider.id.clone(),
                model_id: model.id.clone(),
                label: format!("{} / {}", provider.name, model.name),
                supports_reasoning: !matches!(model.reasoning_profile, ReasoningProfile::None),
            })
        })
        .collect()
}

fn model_provider_view(provider: &ModelProviderResponse) -> ModelProviderView {
    ModelProviderView {
        id: provider.id.clone(),
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        api_format: provider.api_format.to_string(),
        api_key_configured: provider.api_key_configured,
        enabled: provider.enabled,
        read_only: provider.read_only,
        timeout_secs: provider.timeout_secs,
        models: provider.models.iter().map(model_spec).collect(),
    }
}

fn provider_request_from_draft(draft: &ModelProviderDraft) -> ProviderWriteRequest {
    ProviderWriteRequest {
        name: draft.name.clone(),
        base_url: draft.base_url.clone(),
        api_key: secret_update(&draft.api_key),
        enabled: draft.enabled,
        timeout_secs: draft.timeout_secs,
        models: draft.models.iter().map(managed_model).collect(),
        default_model: draft
            .default_model
            .as_ref()
            .map(|default| DefaultModelRequest {
                model_id: default.model_id.clone(),
                reasoning: default.reasoning,
            }),
    }
}

fn model_spec(model: &ManagedModel) -> ManagedModelSpec {
    ManagedModelSpec {
        id: model.id.clone(),
        name: model.name.clone(),
        context_window_tokens: model.context_window_tokens,
        reserved_output_tokens: model.reserved_output_tokens,
        supports_tools: model.supports_tools,
        reasoning_profile: model.reasoning_profile,
    }
}

fn managed_model(model: &ManagedModelSpec) -> ManagedModel {
    ManagedModel {
        id: model.id.clone(),
        name: model.name.clone(),
        context_window_tokens: model.context_window_tokens,
        reserved_output_tokens: model.reserved_output_tokens,
        supports_tools: model.supports_tools,
        reasoning_profile: model.reasoning_profile,
    }
}

fn merge_discovered_models(
    existing: &[ManagedModel],
    discovered: Vec<DiscoveredModel>,
) -> Vec<ManagedModel> {
    let mut models = existing.to_vec();
    for discovered in discovered {
        if models.iter().any(|model| model.id == discovered.id) {
            continue;
        }
        models.push(discovered.suggested.unwrap_or_else(|| ManagedModel {
            name: discovered.id.clone(),
            id: discovered.id,
            context_window_tokens: 128_000,
            reserved_output_tokens: 8_192,
            supports_tools: false,
            reasoning_profile: ReasoningProfile::None,
        }));
    }
    models
}

fn mcp_server_view(server: &McpServerResponse) -> McpServerView {
    McpServerView {
        name: server.name.clone(),
        transport: match server.transport {
            ManagedMcpTransport::Stdio => McpTransport::Stdio,
            ManagedMcpTransport::Http => McpTransport::Http,
        },
        command: server.command.clone(),
        args: server.args.clone(),
        env_keys: server.env_keys.clone(),
        cwd: server.cwd.as_deref().map(PathBuf::from),
        url: server.url.clone(),
        header_keys: server.http_header_keys.clone(),
        endpoint: server
            .command
            .clone()
            .or_else(|| server.url.clone())
            .unwrap_or_default(),
        enabled: server.enabled,
        startup_timeout_secs: server.startup_timeout_sec,
        tool_timeout_secs: server.tool_timeout_sec,
        read_only: server.read_only,
        source: if server.source == "runtime_config" {
            McpServerSource::RuntimeConfig
        } else {
            McpServerSource::MorrowManaged
        },
    }
}

fn mcp_request_from_draft(draft: McpServerDraft) -> McpServerWriteRequest {
    McpServerWriteRequest {
        name: draft.name,
        transport: match draft.transport {
            McpTransport::Stdio => ManagedMcpTransport::Stdio,
            McpTransport::Http => ManagedMcpTransport::Http,
        },
        command: nonempty(draft.command),
        args: draft.args,
        env: secret_map(draft.env),
        cwd: draft.cwd.map(|path| path.display().to_string()),
        url: draft.url.and_then(nonempty),
        http_headers: secret_map(draft.headers),
        enabled: draft.enabled,
        startup_timeout_sec: draft.startup_timeout_secs,
        tool_timeout_sec: draft.tool_timeout_secs,
    }
}

fn mcp_request_from_response(server: &McpServerResponse) -> McpServerWriteRequest {
    McpServerWriteRequest {
        name: server.name.clone(),
        transport: server.transport,
        command: server.command.clone(),
        args: server.args.clone(),
        env: server
            .env_keys
            .iter()
            .map(|key| (key.clone(), None))
            .collect(),
        cwd: server.cwd.clone(),
        url: server.url.clone(),
        http_headers: server
            .http_header_keys
            .iter()
            .map(|key| (key.clone(), None))
            .collect(),
        enabled: server.enabled,
        startup_timeout_sec: server.startup_timeout_sec,
        tool_timeout_sec: server.tool_timeout_sec,
    }
}

fn secret_map(values: BTreeMap<String, SecretValue>) -> BTreeMap<String, Option<String>> {
    values
        .into_iter()
        .map(|(key, value)| {
            let value = (!value.is_empty()).then(|| value.expose().to_string());
            (key, value)
        })
        .collect()
}

fn secret_update(value: &SecretValue) -> Option<String> {
    (!value.is_empty()).then(|| value.expose().to_string())
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn read_avatar_data_url(workspace_root: &Path, path: &Path) -> Result<String, BackendError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    let metadata = fs::metadata(&path)
        .map_err(|error| BackendError::new(format!("无法检查头像 {}：{error}", path.display())))?;
    if !metadata.is_file() {
        return Err(BackendError::new(format!(
            "头像路径不是普通文件：{}",
            path.display()
        )));
    }
    let max_bytes = agent_app::MAX_SUBAGENT_AVATAR_BYTES;
    if metadata.len() > max_bytes as u64 {
        return Err(BackendError::new(format!(
            "头像超过 {} KiB 限制",
            max_bytes / 1024
        )));
    }
    let file = File::open(&path)
        .map_err(|error| BackendError::new(format!("无法读取头像 {}：{error}", path.display())))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| BackendError::new(format!("无法读取头像 {}：{error}", path.display())))?;
    if bytes.len() > max_bytes {
        return Err(BackendError::new(format!(
            "头像超过 {} KiB 限制",
            max_bytes / 1024
        )));
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(BackendError::new(
            "头像必须是已经符合大小限制的 PNG、JPEG 或 WebP 文件",
        ));
    };
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

fn backend_error(error: impl ToString) -> BackendError {
    BackendError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_config::ContextConfig;
    use agent_protocol::{PermissionMode, ReasoningLevel, ShellPolicy, WorkspaceLocation};
    use agent_tui::{DefaultModelDraft, McpServerSource};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_backend(name: &str) -> LocalWorkspaceBackend {
        let root = std::env::temp_dir().join(format!(
            "morrow-tui-backend-{name}-{}-{}",
            agent_runtime::timestamp_ms(),
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test workspace");
        let app = WorkspaceApp::new(agent_app::WorkspaceOptions {
            fallback_model: None,
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
            config_path: None,
            config_diagnostics: Vec::new(),
            permissions: PermissionProfile {
                mode: PermissionMode::ReadOnly,
                shell: ShellPolicy::Deny,
            },
            mcp_servers: Vec::new(),
            default_session_name: "default".to_string(),
            persistent_settings: false,
        })
        .expect("create app");
        LocalWorkspaceBackend::new(app, root)
    }

    fn model(id: &str, context: usize, reserved: usize) -> ManagedModelSpec {
        ManagedModelSpec {
            id: id.to_string(),
            name: format!("Model {id}"),
            context_window_tokens: context,
            reserved_output_tokens: reserved,
            supports_tools: true,
            reasoning_profile: ReasoningProfile::Deepseek,
        }
    }

    #[test]
    fn model_options_exclude_disabled_providers() {
        let provider = |id: &str, enabled: bool| ModelProviderResponse {
            id: id.to_string(),
            name: format!("Provider {id}"),
            base_url: "https://models.example.test/v1".to_string(),
            api_format: "openai_chat_completions",
            enabled,
            read_only: false,
            api_key_configured: true,
            timeout_secs: 120,
            models: vec![managed_model(&model(id, 32_768, 4_096))],
        };

        let options = model_options(&[provider("enabled", true), provider("disabled", false)]);

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].provider_id, "enabled");
        assert_eq!(options[0].model_id, "enabled");
    }

    async fn model_server(ids: &[&str]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind model server");
        let address = listener.local_addr().expect("model server address");
        let body = serde_json::json!({
            "object": "list",
            "data": ids
                .iter()
                .map(|id| serde_json::json!({"id": id, "object": "model"}))
                .collect::<Vec<_>>()
        })
        .to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept model request");
            let mut request = vec![0_u8; 4096];
            let _ = stream.read(&mut request).await.expect("read model request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write model response");
        });
        format!("http://{address}/v1")
    }

    #[test]
    fn empty_secret_means_preserve_existing_value() {
        assert_eq!(secret_update(&SecretValue::new("")), None);
        assert_eq!(
            secret_update(&SecretValue::new("new-key")),
            Some("new-key".to_string())
        );
    }

    #[tokio::test]
    async fn explicit_model_specs_round_trip_without_discovery_or_hardcoded_limits() {
        let backend = test_backend("explicit-model");
        backend
            .save_model_provider(ModelProviderDraft {
                id: String::new(),
                name: "Custom".to_string(),
                base_url: "https://models.example.test/v1".to_string(),
                api_key: SecretValue::new("secret"),
                enabled: true,
                read_only: false,
                timeout_secs: 37,
                models: vec![model("custom-model", 65_536, 4_096)],
                default_model: Some(DefaultModelDraft {
                    model_id: "custom-model".to_string(),
                    reasoning: ReasoningLevel::High,
                }),
            })
            .await
            .expect("save explicit provider");

        let settings = backend.load_settings().await.expect("load settings");
        let provider = settings.providers.first().expect("saved provider");
        assert_eq!(provider.timeout_secs, 37);
        assert!(!provider.read_only);
        assert_eq!(provider.models, [model("custom-model", 65_536, 4_096)]);
        assert_eq!(
            settings.default_model,
            Some(ModelSelection {
                provider_id: provider.id.clone(),
                model_id: "custom-model".to_string(),
                reasoning: ReasoningLevel::High,
            })
        );

        backend
            .save_model_provider(ModelProviderDraft {
                id: provider.id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                api_key: SecretValue::default(),
                enabled: provider.enabled,
                read_only: provider.read_only,
                timeout_secs: 45,
                models: vec![model("custom-model", 96_000, 6_000)],
                default_model: None,
            })
            .await
            .expect("update explicit provider");
        let updated = backend.load_settings().await.expect("reload settings");
        let provider = updated.providers.first().expect("updated provider");
        assert!(provider.api_key_configured);
        assert_eq!(provider.timeout_secs, 45);
        assert_eq!(provider.models[0].context_window_tokens, 96_000);
        assert_eq!(provider.models[0].reserved_output_tokens, 6_000);
    }

    #[tokio::test]
    async fn empty_model_onboarding_discovers_models_and_selects_first_default() {
        let backend = test_backend("model-onboarding");
        let base_url = model_server(&["plain-model"]).await;
        backend
            .save_model_provider(ModelProviderDraft {
                id: String::new(),
                name: "Discovered".to_string(),
                base_url,
                api_key: SecretValue::new("secret"),
                enabled: true,
                read_only: false,
                timeout_secs: 5,
                models: Vec::new(),
                default_model: None,
            })
            .await
            .expect("discover and save provider");

        let settings = backend.load_settings().await.expect("load settings");
        let provider = settings.providers.first().expect("saved provider");
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "plain-model");
        assert_eq!(provider.models[0].context_window_tokens, 128_000);
        assert_eq!(provider.models[0].reserved_output_tokens, 8_192);
        assert_eq!(
            settings
                .default_model
                .as_ref()
                .map(|model| model.model_id.as_str()),
            Some("plain-model")
        );
    }

    #[tokio::test]
    async fn mcp_updates_preserve_empty_secrets_and_delete_omitted_keys() {
        let backend = test_backend("mcp-secrets");
        let mut env = BTreeMap::new();
        env.insert("KEEP".to_string(), SecretValue::new("keep-secret"));
        env.insert("REMOVE".to_string(), SecretValue::new("remove-secret"));
        backend
            .save_mcp_server(McpServerDraft {
                original_name: None,
                name: "stdio-server".to_string(),
                transport: McpTransport::Stdio,
                command: "mcp-server".to_string(),
                args: vec!["--stdio".to_string()],
                cwd: Some(backend.workspace_root.clone()),
                url: None,
                env,
                headers: BTreeMap::new(),
                enabled: true,
                startup_timeout_secs: 12,
                tool_timeout_secs: 34,
                read_only: false,
                source: McpServerSource::MorrowManaged,
            })
            .await
            .expect("create stdio server");

        let mut preserved = BTreeMap::new();
        preserved.insert("KEEP".to_string(), SecretValue::default());
        backend
            .save_mcp_server(McpServerDraft {
                original_name: Some("stdio-server".to_string()),
                name: "renamed-server".to_string(),
                transport: McpTransport::Stdio,
                command: "mcp-server".to_string(),
                args: vec!["--stdio".to_string(), "--verbose".to_string()],
                cwd: Some(backend.workspace_root.clone()),
                url: None,
                env: preserved,
                headers: BTreeMap::new(),
                enabled: false,
                startup_timeout_secs: 20,
                tool_timeout_secs: 80,
                read_only: false,
                source: McpServerSource::MorrowManaged,
            })
            .await
            .expect("update stdio server");

        let settings = backend.load_settings().await.expect("load MCP settings");
        let server = settings.mcp_servers.first().expect("saved MCP server");
        assert_eq!(server.name, "renamed-server");
        assert_eq!(server.args, ["--stdio", "--verbose"]);
        assert_eq!(server.env_keys, ["KEEP"]);
        assert_eq!(
            server.cwd.as_deref(),
            Some(backend.workspace_root.as_path())
        );
        assert_eq!(server.startup_timeout_secs, 20);
        assert_eq!(server.tool_timeout_secs, 80);
        assert!(!server.enabled);
        assert!(!server.read_only);
        assert_eq!(server.source, McpServerSource::MorrowManaged);

        let mut headers = BTreeMap::new();
        headers.insert("KEEP".to_string(), SecretValue::new("keep-header"));
        headers.insert("REMOVE".to_string(), SecretValue::new("remove-header"));
        backend
            .save_mcp_server(McpServerDraft {
                original_name: None,
                name: "http-server".to_string(),
                transport: McpTransport::Http,
                command: String::new(),
                args: Vec::new(),
                cwd: None,
                url: Some("https://mcp.example.test".to_string()),
                env: BTreeMap::new(),
                headers,
                enabled: true,
                startup_timeout_secs: 10,
                tool_timeout_secs: 60,
                read_only: false,
                source: McpServerSource::MorrowManaged,
            })
            .await
            .expect("create HTTP server");
        let mut preserved_headers = BTreeMap::new();
        preserved_headers.insert("KEEP".to_string(), SecretValue::default());
        backend
            .save_mcp_server(McpServerDraft {
                original_name: Some("http-server".to_string()),
                name: "http-server".to_string(),
                transport: McpTransport::Http,
                command: String::new(),
                args: Vec::new(),
                cwd: None,
                url: Some("https://mcp.example.test".to_string()),
                env: BTreeMap::new(),
                headers: preserved_headers,
                enabled: true,
                startup_timeout_secs: 10,
                tool_timeout_secs: 60,
                read_only: false,
                source: McpServerSource::MorrowManaged,
            })
            .await
            .expect("update HTTP server");
        let settings = backend.load_settings().await.expect("reload MCP settings");
        let http = settings
            .mcp_servers
            .iter()
            .find(|server| server.name == "http-server")
            .expect("saved HTTP server");
        assert_eq!(http.header_keys, ["KEEP"]);
    }

    #[tokio::test]
    async fn command_hints_and_subagent_avatar_actions_round_trip() {
        let backend = test_backend("command-avatar");
        backend
            .save_managed_command(ManagedCommandDraft {
                original_name: None,
                name: "review".to_string(),
                description: "Review a file".to_string(),
                argument_hint: "<file-path>".to_string(),
                prompt: "Review $ARGUMENTS".to_string(),
            })
            .await
            .expect("save command");
        let settings = backend
            .load_settings()
            .await
            .expect("load command settings");
        assert_eq!(settings.commands[0].argument_hint, "<file-path>");

        let avatar_path = backend.workspace_root.join("avatar.png");
        fs::write(
            &avatar_path,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .expect("write avatar");
        backend
            .save_subagent_identity(SubagentIdentityDraft {
                original_id: Some("builtin-01".to_string()),
                identity: agent_protocol::SubagentIdentity {
                    id: "builtin-01".to_string(),
                    name: "Avatar User".to_string(),
                },
                avatar_path: Some(avatar_path),
                remove_avatar: false,
            })
            .await
            .expect("save avatar");
        let settings = backend.load_settings().await.expect("load avatar settings");
        let profile = settings
            .subagent_identities
            .iter()
            .find(|profile| profile.identity.id == "builtin-01")
            .expect("updated profile");
        assert!(profile.avatar_configured);
        assert!(profile.avatar_path.is_none());

        backend
            .save_subagent_identity(SubagentIdentityDraft {
                original_id: Some("builtin-01".to_string()),
                identity: profile.identity.clone(),
                avatar_path: None,
                remove_avatar: true,
            })
            .await
            .expect("remove avatar");
        let settings = backend
            .load_settings()
            .await
            .expect("reload avatar settings");
        let profile = settings
            .subagent_identities
            .iter()
            .find(|profile| profile.identity.id == "builtin-01")
            .expect("updated profile");
        assert!(!profile.avatar_configured);
    }

    #[test]
    fn oversized_avatar_is_rejected_from_metadata_before_encoding() {
        let root = std::env::temp_dir().join(format!(
            "morrow-tui-avatar-limit-{}-{}",
            agent_runtime::timestamp_ms(),
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create avatar workspace");
        let path = root.join("too-large.png");
        fs::write(&path, vec![0_u8; agent_app::MAX_SUBAGENT_AVATAR_BYTES + 1])
            .expect("write oversized avatar");

        let error = read_avatar_data_url(&root, &path).expect_err("reject oversized avatar");
        assert!(error.message.contains("超过 256 KiB"));
    }

    #[test]
    fn discovered_models_keep_existing_specs_and_use_safe_compatibility_defaults() {
        let existing = ManagedModel {
            id: "existing".to_string(),
            name: "Existing".to_string(),
            context_window_tokens: 32_000,
            reserved_output_tokens: 2_000,
            supports_tools: true,
            reasoning_profile: ReasoningProfile::None,
        };
        let merged = merge_discovered_models(
            std::slice::from_ref(&existing),
            vec![
                DiscoveredModel {
                    id: "existing".to_string(),
                    suggested: Some(ManagedModel {
                        context_window_tokens: 999_999,
                        ..existing.clone()
                    }),
                },
                DiscoveredModel {
                    id: "new".to_string(),
                    suggested: None,
                },
            ],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], existing);
        assert_eq!(merged[1].context_window_tokens, 128_000);
        assert_eq!(merged[1].reserved_output_tokens, 8_192);
        assert!(!merged[1].supports_tools);
    }

    #[test]
    fn app_events_preserve_session_identity_and_origin() {
        let events = translate_event(
            "work",
            AppEvent::AgentEvent(Box::new(agent_runtime::AgentEventEnvelope {
                schema_version: 1,
                timestamp_ms: 1,
                session: "work".to_string(),
                workspace_root: "/tmp/work".to_string(),
                origin: agent_protocol::AgentEventOrigin::SubagentRun {
                    instance_id: "agent-1".to_string(),
                    run_id: "run-1".to_string(),
                    role: agent_protocol::SubagentRole::Worker,
                    identity_id: None,
                    identity_name: None,
                    turn_index: 0,
                },
                turn_index: 0,
                event_index: 0,
                event: agent_protocol::AgentEvent::TextDelta("hello".to_string()),
            })),
        );

        assert!(matches!(
            events.as_slice(),
            [WorkspaceEvent::Agent {
                session_id,
                origin: agent_protocol::AgentEventOrigin::SubagentRun { instance_id, .. },
                event: agent_protocol::AgentEvent::TextDelta(text),
            }] if session_id == "work" && instance_id == "agent-1" && text == "hello"
        ));
    }

    #[test]
    fn snapshots_restore_pending_approvals_and_subagents() {
        let approval = agent_protocol::ApprovalRequest::shell_command(
            "approval-1",
            "cargo test",
            "/tmp/work",
            60,
            "需要批准",
        );
        let events = translate_event(
            "work",
            AppEvent::Snapshot {
                session: agent_protocol::Session::new(),
                running_turn: None,
                permissions: PermissionProfile::default(),
                subagents: Vec::new(),
                approvals: vec![approval.clone()],
            },
        );

        assert!(events.iter().any(|event| matches!(
            event,
            WorkspaceEvent::ApprovalQueue {
                session_id,
                approvals,
            } if session_id == "work" && approvals.len() == 1 && approvals[0] == approval
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            WorkspaceEvent::SubagentsChanged {
                session_id,
                subagents,
            } if session_id == "work" && subagents.is_empty()
        )));
    }
}
