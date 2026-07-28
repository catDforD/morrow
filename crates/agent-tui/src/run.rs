use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_protocol::PermissionProfile;
use crossterm::{clipboard::CopyToClipboard, event::EventStream, execute};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::backend::{BackendCommand, WorkspaceBackend, WorkspaceEvent, WorkspaceSnapshot};
use crate::persistence::{TuiStateFile, WorkspaceTuiState, default_state_path};
use crate::state::{AppState, Effect, Message};
use crate::{TerminalGuard, TuiError};

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub workspace: PathBuf,
    pub initial_session: InitialSession,
    pub permission_override: Option<PermissionProfile>,
    pub state_path: Option<PathBuf>,
    pub no_color: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            initial_session: InitialSession::ResumeRecent,
            permission_override: None,
            state_path: None,
            no_color: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InitialSession {
    New,
    Named(String),
    #[default]
    ResumeRecent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Exited,
    CancelledActive,
}

pub async fn run<B: WorkspaceBackend>(
    backend: B,
    options: LaunchOptions,
) -> Result<RunOutcome, TuiError> {
    let backend = Arc::new(backend);
    let state_path = options
        .state_path
        .clone()
        .unwrap_or_else(default_state_path);
    let state_file = TuiStateFile::load(&state_path).unwrap_or_default();
    let persisted = state_file.workspace(&options.workspace).cloned();
    let snapshot =
        initial_snapshot(&*backend, &options.initial_session, persisted.as_ref()).await?;
    let mut state = AppState::new(
        options.workspace.clone(),
        snapshot,
        persisted,
        options.permission_override,
        options.no_color,
    );
    if let Ok(settings) = backend.load_settings().await {
        state.models.clone_from(&settings.models);
        state.settings.snapshot = Some(settings);
    }

    let mut terminal = TerminalGuard::enter().map_err(TuiError::Terminal)?;
    let size = terminal.terminal_mut().size().map_err(TuiError::Terminal)?;
    state.terminal_size = (size.width, size.height);
    terminal
        .terminal_mut()
        .draw(|frame| crate::ui::render(frame, &mut state))
        .map_err(TuiError::Terminal)?;
    let result = run_loop(backend, &mut terminal, &mut state, state_path).await;
    let restore_result = terminal.restore();
    match (result, restore_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(TuiError::RuntimeTerminal(error)),
    }
}

async fn initial_snapshot<B: WorkspaceBackend>(
    backend: &B,
    initial_session: &InitialSession,
    persisted: Option<&WorkspaceTuiState>,
) -> Result<WorkspaceSnapshot, TuiError> {
    match initial_session {
        InitialSession::New => {
            let created = backend.execute(BackendCommand::CreateSession).await?;
            let crate::backend::CommandResult::SessionCreated(session) = created else {
                return Err(TuiError::Backend(crate::backend::BackendError::new(
                    "创建新会话时后端返回了无效结果",
                )));
            };
            backend
                .snapshot(Some(&session.info.id))
                .await
                .map_err(TuiError::Backend)
        }
        InitialSession::Named(session_id) => backend
            .snapshot(Some(session_id))
            .await
            .map_err(TuiError::Backend),
        InitialSession::ResumeRecent => backend
            .snapshot(persisted.and_then(|state| state.recent_session.as_deref()))
            .await
            .map_err(TuiError::Backend),
    }
}

async fn run_loop<B: WorkspaceBackend>(
    backend: Arc<B>,
    terminal: &mut TerminalGuard,
    state: &mut AppState,
    state_path: PathBuf,
) -> Result<RunOutcome, TuiError> {
    let mut terminal_events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(33));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (messages_tx, mut messages_rx) = mpsc::unbounded_channel::<Message>();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<BackendCommand>();
    let command_backend = Arc::clone(&backend);
    let command_messages = messages_tx.clone();
    tokio::spawn(async move {
        while let Some(command) = commands_rx.recv().await {
            let result = command_backend.execute(command).await;
            if command_messages
                .send(Message::CommandFinished(result))
                .is_err()
            {
                break;
            }
        }
    });

    loop {
        let message = tokio::select! {
            event = terminal_events.next() => {
                match event {
                    Some(Ok(event)) => Message::Terminal(event),
                    Some(Err(error)) => return Err(TuiError::RuntimeTerminal(error)),
                    None => return Err(TuiError::EventStreamClosed),
                }
            }
            event = backend.recv_event() => match event {
                Ok(event) => Message::Workspace(Ok(event)),
                Err(error) => return Err(TuiError::Backend(error)),
            },
            message = messages_rx.recv() => {
                let Some(message) = message else {
                    return Err(TuiError::EventStreamClosed);
                };
                message
            }
            _ = ticker.tick() => Message::Tick,
        };
        let draw_now = matches!(message, Message::Terminal(_) | Message::Tick);
        let effects = state.update(message);

        if state.should_quit {
            for effect in effects {
                execute_exit_effect(&*backend, effect).await;
            }
            save_state(&state_path, state)?;
            return Ok(if state.cancelled_active_on_exit {
                RunOutcome::CancelledActive
            } else {
                RunOutcome::Exited
            });
        }

        for effect in effects {
            match effect {
                Effect::PersistState => {
                    let result =
                        persist_workspace(&state_path, &state.workspace, state.persisted_state())
                            .map_err(|error| error.to_string());
                    state.update(Message::StatePersisted(result));
                }
                effect => spawn_effect(
                    Arc::clone(&backend),
                    messages_tx.clone(),
                    commands_tx.clone(),
                    effect,
                    state.workspace.clone(),
                    state.active_session_id.clone(),
                ),
            }
        }
        if draw_now {
            terminal
                .terminal_mut()
                .draw(|frame| crate::ui::render(frame, state))
                .map_err(TuiError::RuntimeTerminal)?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_effect<B: WorkspaceBackend>(
    backend: Arc<B>,
    messages: mpsc::UnboundedSender<Message>,
    commands: mpsc::UnboundedSender<BackendCommand>,
    effect: Effect,
    workspace: PathBuf,
    active_session: Option<String>,
) {
    match effect {
        Effect::RefreshSnapshot => {
            tokio::spawn(async move {
                let result = backend.snapshot(active_session.as_deref()).await;
                let _ = messages.send(Message::SnapshotLoaded(result));
            });
        }
        Effect::Backend(command) => {
            if is_control_command(&command) {
                tokio::spawn(async move {
                    let result = backend.execute(command).await;
                    let _ = messages.send(Message::CommandFinished(result));
                });
            } else {
                let _ = commands.send(command);
            }
        }
        Effect::LoadSettings { request_id } => {
            tokio::spawn(async move {
                let result = backend.load_settings().await;
                let _ = messages.send(Message::SettingsLoaded { request_id, result });
            });
        }
        Effect::EstimateContext {
            request_id,
            session_id,
            draft,
            model,
            permissions,
        } => {
            tokio::spawn(async move {
                let result = backend
                    .estimate_context(&session_id, &draft, model, permissions)
                    .await;
                let _ = messages.send(Message::ContextEstimated { request_id, result });
            });
        }
        Effect::CompletePaths {
            request_id,
            query,
            replace,
        } => {
            tokio::spawn(async move {
                let result = backend.complete_paths(&workspace, &query).await;
                let _ = messages.send(Message::PathsCompleted {
                    request_id,
                    replace,
                    result,
                });
            });
        }
        Effect::Copy(text) => {
            let result = execute!(
                io::stdout(),
                CopyToClipboard::to_clipboard_from(text.as_bytes())
            )
            .map_err(|error| error.to_string());
            if let Err(error) = result {
                let _ = messages.send(Message::Workspace(Ok(WorkspaceEvent::Notice(format!(
                    "复制失败: {error}"
                )))));
            }
        }
        Effect::PersistState => {
            unreachable!("persistence is serialized by the event loop")
        }
    }
}

fn is_control_command(command: &BackendCommand) -> bool {
    matches!(
        command,
        BackendCommand::CancelTurn { .. }
            | BackendCommand::ResolveApproval { .. }
            | BackendCommand::CancelSubagent { .. }
    )
}

async fn execute_exit_effect<B: WorkspaceBackend>(backend: &B, effect: Effect) {
    if let Effect::Backend(command) = effect {
        let _ = backend.execute(command).await;
    }
}

fn save_state(path: &std::path::Path, state: &AppState) -> Result<(), TuiError> {
    persist_workspace(path, &state.workspace, state.persisted_state()).map_err(TuiError::State)
}

fn persist_workspace(
    path: &std::path::Path,
    workspace: &std::path::Path,
    persisted: crate::persistence::WorkspaceTuiState,
) -> io::Result<()> {
    let mut file = TuiStateFile::load(path)?;
    file.set_workspace(workspace, persisted);
    file.save_atomic(path)
}

#[allow(dead_code)]
fn _assert_snapshot_send_sync(snapshot: WorkspaceSnapshot) -> WorkspaceSnapshot {
    snapshot
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agent_protocol::{ApprovalDecision, Session};
    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    struct StartupBackend {
        commands: Mutex<Vec<BackendCommand>>,
        snapshots: Mutex<Vec<Option<String>>>,
    }

    fn session_snapshot(id: &str) -> crate::backend::SessionSnapshot {
        crate::backend::SessionSnapshot {
            info: crate::backend::SessionInfo {
                id: id.to_string(),
                title: id.to_string(),
                archived: false,
                running: false,
                model: None,
                permissions: PermissionProfile::default(),
            },
            session: Session::new(),
            subagents: Vec::new(),
            approvals: Vec::new(),
        }
    }

    #[async_trait]
    impl WorkspaceBackend for StartupBackend {
        async fn snapshot(
            &self,
            preferred_session: Option<&str>,
        ) -> Result<WorkspaceSnapshot, crate::backend::BackendError> {
            self.snapshots
                .lock()
                .expect("snapshot lock")
                .push(preferred_session.map(str::to_string));
            let id = preferred_session.unwrap_or("fallback");
            let active = session_snapshot(id);
            Ok(WorkspaceSnapshot {
                sessions: vec![active.info.clone()],
                active_session: Some(active),
                models: Vec::new(),
            })
        }

        async fn recv_event(&self) -> Result<WorkspaceEvent, crate::backend::BackendError> {
            std::future::pending().await
        }

        async fn execute(
            &self,
            command: BackendCommand,
        ) -> Result<crate::backend::CommandResult, crate::backend::BackendError> {
            self.commands
                .lock()
                .expect("command lock")
                .push(command.clone());
            Ok(match command {
                BackendCommand::CreateSession => {
                    crate::backend::CommandResult::SessionCreated(session_snapshot("session-new"))
                }
                _ => crate::backend::CommandResult::Ack,
            })
        }

        async fn load_settings(
            &self,
        ) -> Result<crate::backend::SettingsSnapshot, crate::backend::BackendError> {
            Ok(crate::backend::SettingsSnapshot::default())
        }

        async fn estimate_context(
            &self,
            _session_id: &str,
            _draft: &str,
            _model: Option<agent_protocol::ModelSelection>,
            _permissions: PermissionProfile,
        ) -> Result<crate::backend::ContextEstimate, crate::backend::BackendError> {
            Ok(crate::backend::ContextEstimate::default())
        }
    }

    #[tokio::test]
    async fn initial_session_policy_creates_names_or_resumes_explicitly() {
        let backend = StartupBackend::default();
        let persisted = WorkspaceTuiState {
            recent_session: Some("recent".to_string()),
            ..WorkspaceTuiState::default()
        };

        initial_snapshot(&backend, &InitialSession::New, Some(&persisted))
            .await
            .expect("new session snapshot");
        assert_eq!(
            backend.commands.lock().expect("command lock").as_slice(),
            [BackendCommand::CreateSession]
        );
        assert_eq!(
            backend.snapshots.lock().expect("snapshot lock").as_slice(),
            [Some("session-new".to_string())]
        );

        backend.commands.lock().expect("command lock").clear();
        backend.snapshots.lock().expect("snapshot lock").clear();
        initial_snapshot(
            &backend,
            &InitialSession::Named("named".to_string()),
            Some(&persisted),
        )
        .await
        .expect("named session snapshot");
        assert!(backend.commands.lock().expect("command lock").is_empty());
        assert_eq!(
            backend.snapshots.lock().expect("snapshot lock").as_slice(),
            [Some("named".to_string())]
        );

        backend.snapshots.lock().expect("snapshot lock").clear();
        initial_snapshot(&backend, &InitialSession::ResumeRecent, Some(&persisted))
            .await
            .expect("recent session snapshot");
        assert_eq!(
            backend.snapshots.lock().expect("snapshot lock").as_slice(),
            [Some("recent".to_string())]
        );
    }

    #[test]
    fn approvals_and_cancellation_bypass_the_serial_command_lane() {
        let control_commands = [
            BackendCommand::CancelTurn {
                session_id: "main".to_string(),
            },
            BackendCommand::ResolveApproval {
                session_id: "main".to_string(),
                decision: ApprovalDecision::deny("approval"),
            },
            BackendCommand::CancelSubagent {
                session_id: "main".to_string(),
                instance_id: "subagent".to_string(),
            },
        ];

        assert!(control_commands.iter().all(is_control_command));
        assert!(!is_control_command(&BackendCommand::Settings(
            crate::backend::SettingsCommand::DiscoverModels {
                provider_id: "provider".to_string(),
            },
        )));
    }
}
