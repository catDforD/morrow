use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;

use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalOrigin, ApprovalRequest,
    MAX_SUBAGENT_PROMPT_SUFFIX_CHARS, MAX_SUBAGENT_TIMEOUT_SECS, MAX_SUBAGENT_TOOL_ROUNDS,
    MIN_SUBAGENT_TIMEOUT_SECS, MIN_SUBAGENT_TOOL_ROUNDS, ModelSelection, PermissionProfile,
    ReasoningLevel, SubagentIdentity, SubagentInstanceSnapshot, SubagentRole, SubagentRoleOverride,
    ToolExecutionSummary, TurnStatus,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::backend::{
    BackendCommand, BackendError, CommandResult, ContextEstimate, DefaultModelDraft,
    ManagedCommandDraft, ManagedModelSpec, McpServerDraft, McpServerSource, McpTransport,
    ModelProviderDraft, SecretValue, SessionInfo, SessionSnapshot, SettingsCommand,
    SettingsSnapshot, SubagentIdentityDraft, SubagentRoleView, WorkspaceEvent, WorkspaceSnapshot,
};
use crate::completion::{PathCompletion, path_token};
use crate::input::{Composer, TerminalTextSanitizer, sanitize_input, sanitize_terminal_text};
use crate::persistence::WorkspaceTuiState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
    TooSmall,
}

const CTRL_C_FORCE_EXIT_TICKS: u64 = 31;
const CTRL_C_FORCE_EXIT_HINT: &str = "再次按 Ctrl+C 强制退出";

fn sanitize_tool_summary(mut summary: ToolExecutionSummary) -> ToolExecutionSummary {
    for file in &mut summary.files {
        file.path = sanitize_terminal_text(&file.path);
    }
    summary.diff = summary.diff.map(|value| sanitize_terminal_text(&value));
    if let Some(shell) = &mut summary.shell {
        shell.command = sanitize_terminal_text(&shell.command);
    }
    summary.error = summary.error.map(|value| sanitize_terminal_text(&value));
    if let Some(subagent) = &mut summary.subagent {
        subagent.agent_id = subagent
            .agent_id
            .take()
            .map(|value| sanitize_terminal_text(&value));
        subagent.agent_name = subagent
            .agent_name
            .take()
            .map(|value| sanitize_terminal_text(&value));
        subagent.task = sanitize_terminal_text(&subagent.task);
        subagent.result = subagent
            .result
            .take()
            .map(|value| sanitize_terminal_text(&value));
        subagent.error = subagent
            .error
            .take()
            .map(|value| sanitize_terminal_text(&value));
    }
    summary
}

fn field(label: &str, value: &str, secret: bool) -> FormField {
    FormField {
        label: label.to_string(),
        value: value.to_string(),
        secret,
    }
}

fn model_specs_json(models: &[ManagedModelSpec]) -> String {
    serde_json::to_string_pretty(models).unwrap_or_else(|_| "[]".to_string())
}

fn string_list_json(values: &[String]) -> String {
    serde_json::to_string_pretty(values).unwrap_or_else(|_| "[]".to_string())
}

fn blank_secret_lines(count: usize) -> String {
    "\n".repeat(count.saturating_sub(1))
}

fn parse_model_specs(value: &str) -> Result<Vec<ManagedModelSpec>, String> {
    serde_json::from_str(value).map_err(|error| format!("模型 JSON 无效: {error}"))
}

fn parse_string_list(value: &str, label: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(value).map_err(|error| format!("{label} JSON 无效: {error}"))
}

fn parse_yes_no(value: &str, label: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "1" => Ok(true),
        "no" | "n" | "false" | "0" => Ok(false),
        _ => Err(format!("{label} 必须是 yes 或 no")),
    }
}

fn parse_reasoning(value: &str) -> Result<ReasoningLevel, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ReasoningLevel::Off),
        "high" => Ok(ReasoningLevel::High),
        "max" => Ok(ReasoningLevel::Max),
        _ => Err("推理级别必须是 off、high 或 max".to_string()),
    }
}

fn parse_u64(value: &str, label: &str) -> Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label} 必须是正整数"))
}

fn parse_usize(value: &str, label: &str) -> Result<usize, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label} 必须是正整数"))
}

fn secret_map_from_lines(
    keys_value: &str,
    secrets_value: &str,
    label: &str,
) -> Result<BTreeMap<String, SecretValue>, String> {
    let keys = keys_value
        .lines()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    for key in &keys {
        if !seen.insert(*key) {
            return Err(format!("{label} 名称 {key:?} 重复"));
        }
    }
    let secrets = secrets_value.split('\n').collect::<Vec<_>>();
    if secrets
        .iter()
        .skip(keys.len())
        .any(|secret| !secret.is_empty())
    {
        return Err(format!("{label} 值数量多于名称数量"));
    }
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            let secret = secrets.get(index).copied().unwrap_or_default();
            (key.to_string(), SecretValue::new(secret))
        })
        .collect())
}

fn delete_command(target: DeleteTarget) -> SettingsCommand {
    match target {
        DeleteTarget::ModelProvider(provider_id) => {
            SettingsCommand::DeleteModelProvider { provider_id }
        }
        DeleteTarget::McpServer(name) => SettingsCommand::DeleteMcpServer { name },
        DeleteTarget::ManagedCommand(name) => SettingsCommand::DeleteManagedCommand { name },
        DeleteTarget::SubagentIdentity(id) => SettingsCommand::DeleteSubagentIdentity { id },
        DeleteTarget::ResetSubagentRoles => SettingsCommand::ResetSubagentRoles,
        DeleteTarget::ResetSubagentProfiles => SettingsCommand::ResetSubagentProfiles,
        DeleteTarget::SubagentInstance { .. } => {
            unreachable!("subagent instances use BackendCommand::DeleteSubagent")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{
        ApprovalOrigin, Message as ProtocolMessage, Session, ShellCommandSummary, ShellPolicy,
        Turn, TurnRecord,
    };

    fn session_snapshot(id: &str, running: bool) -> SessionSnapshot {
        SessionSnapshot {
            info: SessionInfo {
                id: id.to_string(),
                title: id.to_string(),
                archived: false,
                running,
                model: Some(ModelSelection {
                    provider_id: "provider".to_string(),
                    model_id: "model".to_string(),
                    reasoning: ReasoningLevel::Off,
                }),
                permissions: PermissionProfile::default(),
            },
            session: Session::new(),
            subagents: Vec::new(),
            approvals: Vec::new(),
        }
    }

    fn app(running: bool) -> AppState {
        let active = session_snapshot("work", running);
        AppState::new(
            PathBuf::from("/workspace"),
            WorkspaceSnapshot {
                sessions: vec![active.info.clone()],
                active_session: Some(active),
                models: vec![crate::backend::ModelOption {
                    provider_id: "provider".to_string(),
                    model_id: "model".to_string(),
                    label: "Model".to_string(),
                    supports_reasoning: true,
                }],
            },
            None,
            None,
            true,
        )
    }

    fn key(code: KeyCode) -> Message {
        Message::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> Message {
        Message::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn completed_turn(prompt: &str, answer: &str) -> TurnRecord {
        let user = ProtocolMessage::user(prompt);
        let assistant = ProtocolMessage::assistant(answer);
        let mut turn = Turn::running(user.clone());
        turn.complete(assistant.clone());
        TurnRecord::new(turn, vec![user, assistant])
    }

    #[test]
    fn layout_breakpoints_include_extremely_small_terminals() {
        assert_eq!(LayoutMode::for_size(160, 40), LayoutMode::Wide);
        assert_eq!(LayoutMode::for_size(100, 30), LayoutMode::Medium);
        assert_eq!(LayoutMode::for_size(70, 24), LayoutMode::Narrow);
        assert_eq!(LayoutMode::for_size(40, 10), LayoutMode::TooSmall);
    }

    #[test]
    fn approval_never_uses_enter_as_approval() {
        let mut state = app(true);
        state.approvals.push_back(PendingApproval {
            session_id: "work".to_string(),
            request: ApprovalRequest::shell_command("approval", "pwd", "/workspace", 10, "test"),
        });
        assert!(state.update(key(KeyCode::Enter)).is_empty());
        assert_eq!(state.approvals.len(), 1);

        let effects = state.update(key(KeyCode::Char('y')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::ResolveApproval { decision, .. })]
                if decision.approved
        ));
        assert_eq!(state.approval_scroll, 0);
    }

    #[test]
    fn approval_keys_scroll_without_resolving() {
        let mut state = app(true);
        state.approvals.push_back(PendingApproval {
            session_id: "work".to_string(),
            request: ApprovalRequest::shell_command("approval", "pwd", "/workspace", 10, "test"),
        });

        assert!(state.update(key(KeyCode::Down)).is_empty());
        assert_eq!(state.approval_scroll, 1);
        assert!(state.update(key(KeyCode::PageDown)).is_empty());
        assert_eq!(state.approval_scroll, 11);
        assert!(state.update(key(KeyCode::Up)).is_empty());
        assert_eq!(state.approval_scroll, 10);
        assert!(state.update(key(KeyCode::PageUp)).is_empty());
        assert_eq!(state.approval_scroll, 0);
        assert!(state.update(key(KeyCode::End)).is_empty());
        assert_eq!(state.approval_scroll, u16::MAX);
        assert!(state.update(key(KeyCode::Down)).is_empty());
        assert_eq!(state.approval_scroll, u16::MAX);
        assert!(state.update(key(KeyCode::Home)).is_empty());
        assert_eq!(state.approval_scroll, 0);
        assert_eq!(state.approvals.len(), 1);
    }

    #[test]
    fn ctrl_c_targets_background_subagent_approval() {
        let mut state = app(true);
        state.approvals.push_back(PendingApproval {
            session_id: "background".to_string(),
            request: ApprovalRequest::shell_command("same", "pwd", "/workspace", 10, "test")
                .with_origin(ApprovalOrigin::SubagentRun {
                    instance_id: "agent-1".to_string(),
                    run_id: "run-1".to_string(),
                    role: SubagentRole::Worker,
                    identity_id: None,
                    identity_name: None,
                    tool_call_id: None,
                }),
        });
        let effects = state.update(Message::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::CancelSubagent { session_id, instance_id })]
                if session_id == "background" && instance_id == "agent-1"
        ));
        assert_eq!(state.approvals.len(), 1);
    }

    #[test]
    fn ctrl_c_targets_unknown_approval_session_instead_of_active_session() {
        let mut state = app(true);
        state.approvals.push_back(PendingApproval {
            session_id: "background".to_string(),
            request: ApprovalRequest::shell_command(
                "legacy",
                "pwd",
                "/workspace",
                10,
                "legacy request",
            ),
        });

        let effects = state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(
            effects,
            vec![Effect::Backend(BackendCommand::CancelTurn {
                session_id: "background".to_string(),
            })]
        );
    }

    #[test]
    fn second_ctrl_c_within_one_second_cancels_everything_and_exits() {
        let mut state = app(true);

        let first = state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!state.should_quit);
        assert_eq!(state.status.as_deref(), Some(CTRL_C_FORCE_EXIT_HINT));
        assert!(matches!(
            first.as_slice(),
            [Effect::Backend(BackendCommand::CancelTurn { session_id })]
                if session_id == "work"
        ));

        let second = state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(state.should_quit);
        assert!(state.cancelled_active_on_exit);
        assert!(second.iter().any(|effect| matches!(
            effect,
            Effect::Backend(BackendCommand::CancelTurn { session_id }) if session_id == "work"
        )));
        assert!(second.contains(&Effect::PersistState));
    }

    #[test]
    fn repeated_ctrl_c_key_event_does_not_trigger_force_exit() {
        let mut state = app(true);
        state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        let repeat = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        );
        assert!(
            state
                .update(Message::Terminal(Event::Key(repeat)))
                .is_empty()
        );
        assert!(!state.should_quit);
    }

    #[test]
    fn ctrl_c_force_exit_window_expires_after_one_second() {
        let mut state = app(false);
        state.composer.set("draft");
        state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(state.composer.text().is_empty());

        for _ in 0..=CTRL_C_FORCE_EXIT_TICKS {
            state.update(Message::Tick);
        }
        assert!(state.ctrl_c_armed_until_tick.is_none());

        state.update(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!state.should_quit);
        assert!(state.ctrl_c_armed_until_tick.is_some());
    }

    #[test]
    fn global_pages_replace_sidebars_at_every_width() {
        let mut state = app(false);
        state.terminal_size = (160, 40);

        state.update(modified_key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(state.page, MainPage::Sessions);
        state.update(key(KeyCode::Esc));
        assert_eq!(state.page, MainPage::Chat);

        state.update(modified_key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(state.page, MainPage::Inspector);
        state.update(key(KeyCode::Esc));
        assert_eq!(state.page, MainPage::Chat);
    }

    #[test]
    fn bottom_panels_return_to_their_parent_and_approval_has_priority() {
        let mut state = app(false);
        state.update(modified_key(KeyCode::Char(','), KeyModifiers::CONTROL));
        assert_eq!(state.page, MainPage::Settings);
        assert_eq!(state.bottom_panel(), BottomPanel::Settings);

        state.update(key(KeyCode::Char('n')));
        assert!(matches!(state.overlay, Some(Overlay::SettingsEditor(_))));
        assert_eq!(state.bottom_panel(), BottomPanel::SettingsEditor);

        state.update(key(KeyCode::Esc));
        assert!(state.overlay.is_none());
        assert_eq!(state.page, MainPage::Settings);

        state.overlay = Some(Overlay::Help);
        state.approvals.push_back(PendingApproval {
            session_id: "work".to_string(),
            request: ApprovalRequest::shell_command(
                "approval",
                "cargo test",
                "/workspace",
                60,
                "run tests",
            ),
        });
        assert_eq!(state.bottom_panel(), BottomPanel::Approval);

        state.approvals.clear();
        assert_eq!(state.bottom_panel(), BottomPanel::Help);
        state.update(key(KeyCode::Esc));
        assert_eq!(state.bottom_panel(), BottomPanel::Settings);
        state.update(key(KeyCode::Esc));
        assert_eq!(state.page, MainPage::Chat);
    }

    #[test]
    fn running_session_rejects_duplicate_without_losing_draft() {
        let mut state = app(true);
        state.composer.set("保留这份草稿");
        assert!(state.update(key(KeyCode::Enter)).is_empty());
        assert_eq!(state.composer.text(), "保留这份草稿");
    }

    #[test]
    fn alt_arrows_select_chat_turn_and_y_copies_plain_text() {
        let mut state = app(false);
        let mut snapshot = session_snapshot("work", false);
        snapshot
            .session
            .apply_turn(completed_turn("第一问", "第一答"));
        snapshot
            .session
            .apply_turn(completed_turn("第二问", "第二答"));
        state.update(Message::CommandFinished(Ok(CommandResult::Session(
            snapshot,
        ))));

        state.update(modified_key(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(state.active_view().unwrap().selected_message, 1);
        assert_eq!(
            state.update(key(KeyCode::Char('Y'))),
            vec![Effect::Copy("第二问\n\n第二答".to_string())]
        );

        state.update(modified_key(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(state.active_view().unwrap().selected_message, 0);
    }

    #[test]
    fn double_slash_is_forwarded_for_registry_literal_resolution() {
        let mut state = app(false);
        state.composer.set("//review literal");
        let effects = state.update(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::StartTurn { prompt, .. })]
                if prompt == "//review literal"
        ));
    }

    #[test]
    fn chat_paste_multiline_keys_and_managed_command_completion_work_together() {
        let mut state = app(false);
        state.settings.snapshot = Some(SettingsSnapshot {
            commands: vec![crate::backend::ManagedCommandView {
                name: "review".to_string(),
                description: "审查改动".to_string(),
                argument_hint: "[scope]".to_string(),
                prompt: "Review $ARGUMENTS".to_string(),
            }],
            ..SettingsSnapshot::default()
        });

        state.update(Message::Terminal(Event::Paste(
            "第一行\r\n第二行\u{1b}".to_string(),
        )));
        state.update(modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        state.update(modified_key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(state.composer.text(), "第一行\n第二行\n\n");

        state.composer.clear();
        state.update(key(KeyCode::Char('/')));
        assert!(matches!(
            state.completion,
            Some(CompletionPopup {
                kind: CompletionKind::ManagedCommand,
                ..
            })
        ));
        state.update(key(KeyCode::Char('/')));
        assert_eq!(state.composer.text(), "//");
        assert!(state.completion.is_none());
    }

    #[test]
    fn turn_saved_reconciliation_preserves_scroll_and_unread() {
        let mut state = app(false);
        let view = state.active_view_mut().unwrap();
        view.scroll = 7;
        view.at_bottom = false;
        view.unread = 3;
        state.context_debounce = None;
        assert_eq!(
            state.update(Message::Workspace(Ok(WorkspaceEvent::TurnSaved {
                session_id: "work".to_string(),
            }))),
            vec![Effect::Backend(BackendCommand::LoadSession {
                session_id: "work".to_string(),
            })]
        );
        let snapshot = session_snapshot("work", false);
        state.update(Message::CommandFinished(Ok(CommandResult::Session(
            snapshot,
        ))));
        let view = state.active_view().unwrap();
        assert_eq!(view.scroll, 7);
        assert!(!view.at_bottom);
        assert_eq!(view.unread, 3);
        assert_eq!(state.context_debounce, Some(1));
    }

    #[test]
    fn stale_async_results_are_discarded_and_new_draft_requests_are_emitted() {
        let mut state = app(false);
        state.request_id = 20;
        state.context_request = 19;
        state.path_request = 18;
        state.settings_request = 17;
        state.composer.set("@src");
        state.draft_changed();

        state.update(Message::ContextEstimated {
            request_id: 19,
            result: Ok(ContextEstimate {
                used_tokens: 999,
                input_budget_tokens: 1_000,
                auto_compact_at_tokens: 800,
            }),
        });
        state.update(Message::PathsCompleted {
            request_id: 18,
            replace: 1..4,
            result: Ok(vec![PathCompletion {
                path: "stale.rs".to_string(),
                directory: false,
            }]),
        });
        state.update(Message::SettingsLoaded {
            request_id: 16,
            result: Ok(SettingsSnapshot::default()),
        });
        assert!(state.context.estimate.is_none());
        assert!(state.completion.is_none());
        assert!(state.settings.snapshot.is_none());
        assert_eq!(state.models.len(), 1);

        let mut emitted = Vec::new();
        for _ in 0..9 {
            emitted.extend(state.update(Message::Tick));
        }
        assert!(emitted.iter().any(|effect| matches!(
            effect,
            Effect::CompletePaths { query, request_id, .. }
                if query == "src" && *request_id != 18
        )));
        assert!(emitted.iter().any(|effect| matches!(
            effect,
            Effect::EstimateContext { draft, request_id, .. }
                if draft == "@src" && *request_id != 19
        )));
    }

    #[test]
    fn broadcast_lag_requests_an_authoritative_snapshot() {
        let mut state = app(false);
        assert_eq!(
            state.update(Message::Workspace(Ok(WorkspaceEvent::BroadcastLagged))),
            vec![Effect::RefreshSnapshot]
        );
    }

    #[test]
    fn exit_confirmation_accepts_the_displayed_uppercase_keys() {
        let mut waiting = app(true);
        waiting.overlay = Some(Overlay::ExitConfirm);
        assert!(waiting.update(key(KeyCode::Char('W'))).is_empty());
        assert!(waiting.exit_when_idle);

        let mut cancelling = app(true);
        cancelling.overlay = Some(Overlay::ExitConfirm);
        let effects = cancelling.update(key(KeyCode::Char('X')));
        assert!(cancelling.should_quit);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Backend(BackendCommand::CancelTurn { session_id }) if session_id == "work"
        )));

        let mut idle = app(false);
        idle.overlay = Some(Overlay::ExitConfirm);
        idle.update(key(KeyCode::Char('Q')));
        assert!(idle.should_quit);
    }

    #[test]
    fn approval_ids_are_scoped_by_session_and_refresh_keeps_fifo() {
        let mut state = app(false);
        let request_a = ApprovalRequest::shell_command("same", "a", "/workspace", 10, "a");
        let request_b = ApprovalRequest::shell_command("same", "b", "/workspace", 10, "b");
        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "a".to_string(),
            approvals: vec![request_a.clone()],
        })));
        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "b".to_string(),
            approvals: vec![request_b],
        })));
        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "a".to_string(),
            approvals: vec![request_a],
        })));
        assert_eq!(state.approvals.len(), 2);
        assert_eq!(state.approvals[0].session_id, "a");
        assert_eq!(state.approvals[1].session_id, "b");
    }

    #[test]
    fn approval_scroll_resets_only_when_fifo_head_changes() {
        let mut state = app(false);
        let request_a = ApprovalRequest::shell_command("a", "a", "/workspace", 10, "a");
        let request_b = ApprovalRequest::shell_command("b", "b", "/workspace", 10, "b");
        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "work".to_string(),
            approvals: vec![request_a.clone(), request_b.clone()],
        })));
        state.approval_scroll = 17;

        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "work".to_string(),
            approvals: vec![request_a, request_b.clone()],
        })));
        assert_eq!(state.approval_scroll, 17);

        state.update(Message::Workspace(Ok(WorkspaceEvent::ApprovalQueue {
            session_id: "work".to_string(),
            approvals: vec![request_b],
        })));
        assert_eq!(state.approval_scroll, 0);

        state.approval_scroll = 9;
        let effects = state.update(key(KeyCode::Char('n')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::ResolveApproval { decision, .. })]
                if !decision.approved
        ));
        assert_eq!(state.approval_scroll, 0);
    }

    #[test]
    fn approval_resolution_event_resets_scroll_when_head_changes() {
        let mut state = app(false);
        state.approvals.extend([
            PendingApproval {
                session_id: "work".to_string(),
                request: ApprovalRequest::shell_command("first", "a", "/workspace", 10, "a"),
            },
            PendingApproval {
                session_id: "work".to_string(),
                request: ApprovalRequest::shell_command("second", "b", "/workspace", 10, "b"),
            },
        ]);
        state.approval_scroll = 12;

        state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
            session_id: "work".to_string(),
            origin: AgentEventOrigin::Session,
            event: AgentEvent::ApprovalResolved(ApprovalDecision::deny("first")),
        })));

        assert_eq!(state.approvals.front().unwrap().request.id, "second");
        assert_eq!(state.approval_scroll, 0);
    }

    #[test]
    fn startup_permission_override_is_not_persisted() {
        let mut state = app(false);
        state.permission_override = Some(PermissionProfile {
            mode: agent_protocol::PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Allow,
        });
        state.permissions = state.permission_override.unwrap();
        assert_ne!(state.persisted_state().permissions, state.permissions);
    }

    #[test]
    fn cycling_to_model_without_reasoning_resets_session_preference() {
        let mut state = app(false);
        state.models = vec![
            crate::backend::ModelOption {
                provider_id: "provider".to_string(),
                model_id: "model".to_string(),
                label: "Reasoning model".to_string(),
                supports_reasoning: true,
            },
            crate::backend::ModelOption {
                provider_id: "provider".to_string(),
                model_id: "plain".to_string(),
                label: "Plain model".to_string(),
                supports_reasoning: false,
            },
        ];
        state.reasoning = ReasoningLevel::High;
        state.session_preferences.insert(
            "work".to_string(),
            SessionPreference {
                permissions: state.permissions,
                reasoning: ReasoningLevel::High,
            },
        );

        let effects = state.cycle_model();

        assert_eq!(state.reasoning, ReasoningLevel::Off);
        assert_eq!(
            state.session_preferences["work"].reasoning,
            ReasoningLevel::Off
        );
        assert_eq!(
            state.current_model().unwrap().reasoning,
            ReasoningLevel::Off
        );
        assert!(effects.contains(&Effect::PersistState));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Backend(BackendCommand::SetSessionModel { selection, .. })
                if selection.model_id == "plain" && selection.reasoning == ReasoningLevel::Off
        )));
    }

    #[test]
    fn failed_turn_history_is_not_required_for_active_context() {
        let mut state = app(false);
        let mut snapshot = session_snapshot("work", false);
        snapshot
            .session
            .turns
            .push(agent_protocol::TurnRecord::failed_user_prompt(
                "bad",
                "audit failure",
            ));
        state.update(Message::CommandFinished(Ok(CommandResult::Session(
            snapshot,
        ))));
        assert_eq!(
            state
                .active_view()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .session
                .active_thread
                .messages,
            Vec::<ProtocolMessage>::new()
        );
    }

    #[test]
    fn streamed_terminal_sequences_are_sanitized_across_deltas() {
        let mut state = app(true);
        for event in [
            AgentEvent::TextDelta("前🙂\u{1b}".to_string()),
            AgentEvent::TextDelta("[31".to_string()),
            AgentEvent::TextDelta("m红\u{1b}]0;ti".to_string()),
            AgentEvent::TextDelta("tle\u{1b}".to_string()),
            AgentEvent::TextDelta("\\后\n\t".to_string()),
            AgentEvent::ReasoningDelta("推理\u{0090}hidden".to_string()),
            AgentEvent::ReasoningDelta("\u{009c}完成".to_string()),
        ] {
            state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
                session_id: "work".to_string(),
                origin: AgentEventOrigin::Session,
                event,
            })));
        }

        let live = &state.active_view().unwrap().live;
        assert_eq!(live.text, "前🙂红后\n\t");
        assert_eq!(live.reasoning, "推理完成");
    }

    #[test]
    fn finished_tools_keep_structured_sanitized_summaries() {
        let mut state = app(true);
        for event in [
            AgentEvent::ToolCallStarted {
                id: "call-1".to_string(),
                name: "shell".to_string(),
            },
            AgentEvent::ToolCallFinished {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                ok: true,
                summary: Some(ToolExecutionSummary::shell(ShellCommandSummary {
                    command: "printf safe\u{1b}[31m".to_string(),
                    exit_code: Some(0),
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })),
            },
        ] {
            state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
                session_id: "work".to_string(),
                origin: AgentEventOrigin::Session,
                event,
            })));
        }

        let tool = &state.active_view().unwrap().live.tools[0];
        assert_eq!(tool.status, TurnStatus::Completed);
        assert_eq!(
            tool.summary
                .as_ref()
                .and_then(|summary| summary.shell.as_ref())
                .map(|shell| shell.command.as_str()),
            Some("printf safe")
        );
        assert!(!tool.plain_text().contains('\u{1b}'));
    }

    #[test]
    fn terminal_sanitizer_state_is_independent_and_resets_for_new_turn() {
        let mut state = app(true);
        state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
            session_id: "work".to_string(),
            origin: AgentEventOrigin::Session,
            event: AgentEvent::ReasoningDelta("可见\u{1b}]hidden".to_string()),
        })));
        state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
            session_id: "work".to_string(),
            origin: AgentEventOrigin::Session,
            event: AgentEvent::TextDelta("正文".to_string()),
        })));
        assert_eq!(state.active_view().unwrap().live.reasoning, "可见");
        assert_eq!(state.active_view().unwrap().live.text, "正文");

        state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
            session_id: "work".to_string(),
            origin: AgentEventOrigin::Session,
            event: AgentEvent::TurnStarted,
        })));
        state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
            session_id: "work".to_string(),
            origin: AgentEventOrigin::Session,
            event: AgentEvent::ReasoningDelta("新推理".to_string()),
        })));

        let live = &state.active_view().unwrap().live;
        assert_eq!(live.reasoning, "新推理");
        assert!(live.text.is_empty());
    }

    #[test]
    fn subagent_turn_events_do_not_pollute_parent_live_turn() {
        let mut state = app(false);
        let instance = SubagentInstanceSnapshot {
            id: "agent-1".to_string(),
            role: SubagentRole::Worker,
            identity: SubagentIdentity {
                id: "identity-1".to_string(),
                name: "Worker".to_string(),
            },
            status: agent_protocol::SubagentInstanceStatus::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            latest_run_id: Some("run-1".to_string()),
            latest_task: Some("检查代码".to_string()),
            queue_reason: None,
            latest_summary: None,
            event_log_truncated: false,
        };
        let view = state.active_view_mut().unwrap();
        view.subagents.insert(instance.id.clone(), instance);
        view.live.text = "父会话内容".to_string();

        let origin = AgentEventOrigin::SubagentRun {
            instance_id: "agent-1".to_string(),
            run_id: "run-1".to_string(),
            role: SubagentRole::Worker,
            identity_id: Some("identity-1".to_string()),
            identity_name: Some("Worker".to_string()),
            turn_index: 0,
        };
        for event in [
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta("子代理输出".to_string()),
            AgentEvent::TurnCompleted,
        ] {
            state.update(Message::Workspace(Ok(WorkspaceEvent::Agent {
                session_id: "work".to_string(),
                origin: origin.clone(),
                event,
            })));
        }

        let live = &state.active_view().unwrap().live;
        assert_eq!(live.text, "父会话内容");
        assert!(!live.awaiting_save);
        assert!(!state.sessions[0].running);
        let transcript = &state.subagent_transcripts["agent-1"];
        assert_eq!(transcript.lines.len(), 3);
        assert!(transcript.lines[1].contains("子代理输出"));
    }

    #[test]
    fn model_editor_preserves_full_specs_and_default_selection() {
        let state = app(false);
        let editor = SettingsEditor {
            title: "模型".to_string(),
            kind: EditorKind::ModelProvider { original_id: None },
            fields: vec![
                field("名称", "DeepSeek", false),
                field("Base URL", "https://api.example.com/v1", false),
                field("API Key", "secret", true),
                field("启用", "yes", false),
                field("超时", "120", false),
                field(
                    "模型",
                    r#"[{"id":"model","name":"Model","context_window_tokens":128000,"reserved_output_tokens":8192,"supports_tools":true,"reasoning_profile":"deepseek"}]"#,
                    false,
                ),
                field("默认", "model", false),
                field("推理", "high", false),
            ],
            selected: 0,
        };
        let SettingsCommand::SaveModelProvider(draft) =
            state.settings_command_from_editor(&editor).unwrap()
        else {
            panic!("model provider command expected");
        };
        assert_eq!(draft.models[0].context_window_tokens, 128_000);
        assert!(draft.models[0].supports_tools);
        assert_eq!(draft.default_model.unwrap().reasoning, ReasoningLevel::High);
    }

    #[test]
    fn mcp_editor_preserves_empty_secrets_and_ctrl_t_tests_draft() {
        let mut state = app(false);
        state.settings.snapshot = Some(SettingsSnapshot {
            mcp_servers: vec![crate::backend::McpServerView {
                name: "docs".to_string(),
                transport: McpTransport::Http,
                command: None,
                args: Vec::new(),
                env_keys: vec!["TOKEN".to_string()],
                cwd: None,
                url: Some("https://example.com/mcp".to_string()),
                header_keys: vec!["Authorization".to_string()],
                endpoint: "https://example.com/mcp".to_string(),
                enabled: true,
                startup_timeout_secs: 10,
                tool_timeout_secs: 60,
                read_only: false,
                source: McpServerSource::MorrowManaged,
            }],
            ..SettingsSnapshot::default()
        });
        let mut editor = SettingsEditor {
            title: "MCP".to_string(),
            kind: EditorKind::McpServer {
                original_name: Some("docs".to_string()),
            },
            fields: vec![
                field("名称", "docs", false),
                field("传输", "http", false),
                field("命令", "", false),
                field("参数", "[]", false),
                field("cwd", "", false),
                field("URL", "https://example.com/mcp", false),
                field("Env keys", "TOKEN", false),
                field("Env values", "", true),
                field("Header keys", "Authorization", false),
                field("Header values", "", true),
                field("启用", "yes", false),
                field("启动", "10", false),
                field("工具", "60", false),
            ],
            selected: 0,
        };
        let SettingsCommand::SaveMcpServer(draft) =
            state.settings_command_from_editor(&editor).unwrap()
        else {
            panic!("MCP save command expected");
        };
        assert!(draft.env["TOKEN"].is_empty());
        assert!(draft.headers["Authorization"].is_empty());
        let effects = state.handle_editor_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::Settings(
                SettingsCommand::TestMcpServerDraft(_)
            ))]
        ));
    }

    #[test]
    fn command_editor_paste_keeps_multiline_prompt_and_argument_hint() {
        let mut state = app(false);
        state.overlay = Some(Overlay::SettingsEditor(SettingsEditor {
            title: "命令".to_string(),
            kind: EditorKind::ManagedCommand {
                original_name: None,
            },
            fields: vec![
                field("名称", "review", false),
                field("说明", "Review", false),
                field("参数提示", "<path>", false),
                field("Prompt", "", false),
            ],
            selected: 3,
        }));
        state.update(Message::Terminal(Event::Paste(
            "第一行\n第二行".to_string(),
        )));
        let Overlay::SettingsEditor(editor) = state.overlay.clone().unwrap() else {
            panic!("settings editor expected");
        };
        let SettingsCommand::SaveManagedCommand(draft) =
            state.settings_command_from_editor(&editor).unwrap()
        else {
            panic!("managed command expected");
        };
        assert_eq!(draft.argument_hint, "<path>");
        assert_eq!(draft.prompt, "第一行\n第二行");
    }

    #[test]
    fn avatar_remove_and_subagent_resets_are_explicit() {
        let mut state = app(false);
        let editor = SettingsEditor {
            title: "身份".to_string(),
            kind: EditorKind::SubagentIdentity {
                original_id: Some("agent-1".to_string()),
                avatar_configured: true,
            },
            fields: vec![
                field("名称", "Reviewer", false),
                field("头像", "", false),
                field("移除", "yes", false),
            ],
            selected: 0,
        };
        let SettingsCommand::SaveSubagentIdentity(draft) =
            state.settings_command_from_editor(&editor).unwrap()
        else {
            panic!("subagent identity expected");
        };
        assert!(draft.remove_avatar);

        state.page = MainPage::Settings;
        state.settings.section = SettingsSection::Subagents;
        state.update(key(KeyCode::Char('R')));
        assert_eq!(
            state.overlay,
            Some(Overlay::ConfirmDelete(DeleteTarget::ResetSubagentRoles))
        );
        state.overlay = None;
        state.update(key(KeyCode::Char('P')));
        assert_eq!(
            state.overlay,
            Some(Overlay::ConfirmDelete(DeleteTarget::ResetSubagentProfiles))
        );
    }
}

impl LayoutMode {
    pub fn for_size(width: u16, height: u16) -> Self {
        if width < 48 || height < 12 {
            Self::TooSmall
        } else if width >= 140 {
            Self::Wide
        } else if width >= 90 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MainPage {
    #[default]
    Chat,
    Sessions,
    Inspector,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomPanel {
    Composer,
    Sessions,
    Inspector,
    Settings,
    Approval,
    Help,
    ActionPalette,
    ExitConfirm,
    ConfirmDelete,
    SettingsEditor,
    SubagentFollowUp,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InspectorTab {
    #[default]
    Run,
    Subagents,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingsSection {
    #[default]
    Models,
    Mcp,
    Commands,
    Subagents,
}

impl SettingsSection {
    pub const ALL: [Self; 4] = [Self::Models, Self::Mcp, Self::Commands, Self::Subagents];

    fn previous(self) -> Self {
        match self {
            Self::Models => Self::Subagents,
            Self::Mcp => Self::Models,
            Self::Commands => Self::Mcp,
            Self::Subagents => Self::Commands,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Models => Self::Mcp,
            Self::Mcp => Self::Commands,
            Self::Commands => Self::Subagents,
            Self::Subagents => Self::Models,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    ModelProvider(String),
    McpServer(String),
    ManagedCommand(String),
    SubagentIdentity(String),
    ResetSubagentRoles,
    ResetSubagentProfiles,
    SubagentInstance {
        session_id: String,
        instance_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorKind {
    ModelProvider {
        original_id: Option<String>,
    },
    McpServer {
        original_name: Option<String>,
    },
    McpImport,
    ManagedCommand {
        original_name: Option<String>,
    },
    SubagentIdentity {
        original_id: Option<String>,
        avatar_configured: bool,
    },
    SubagentRole(SubagentRole),
}

#[derive(Clone, PartialEq, Eq)]
pub struct FormField {
    pub label: String,
    pub value: String,
    pub secret: bool,
}

impl std::fmt::Debug for FormField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FormField")
            .field("label", &self.label)
            .field(
                "value",
                &if self.secret {
                    "<redacted>"
                } else {
                    &self.value
                },
            )
            .field("secret", &self.secret)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsEditor {
    pub title: String,
    pub kind: EditorKind,
    pub fields: Vec<FormField>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    ActionPalette { selected: usize },
    ExitConfirm,
    ConfirmDelete(DeleteTarget),
    SettingsEditor(SettingsEditor),
    SubagentFollowUp { instance_id: String, value: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    ManagedCommand,
    WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub replacement: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPopup {
    pub kind: CompletionKind,
    pub replace: Range<usize>,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub session_id: String,
    pub request: ApprovalRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRun {
    pub id: String,
    pub name: String,
    pub status: TurnStatus,
    pub summary: Option<ToolExecutionSummary>,
    pub result: Option<String>,
}

impl ToolRun {
    pub fn plain_text(&self) -> String {
        let mut text = format!("工具 {}", self.name);
        if let Some(summary) = &self.summary {
            for file in &summary.files {
                text.push_str(&format!("\n{} {}", file.operation.as_str(), file.path));
            }
            if let Some(shell) = &summary.shell {
                text.push_str(&format!("\n$ {}", shell.command));
                if let Some(exit_code) = shell.exit_code {
                    text.push_str(&format!("\nexit {exit_code}"));
                }
                if shell.timed_out {
                    text.push_str("\n已超时");
                }
            }
            if let Some(subagent) = &summary.subagent {
                text.push_str(&format!("\n任务: {}", subagent.task));
                if let Some(result) = &subagent.result {
                    text.push('\n');
                    text.push_str(result);
                }
                if let Some(error) = &subagent.error {
                    text.push_str("\n错误: ");
                    text.push_str(error);
                }
            }
            if let Some(diff) = &summary.diff {
                text.push_str("\n\n");
                text.push_str(diff);
            }
            if let Some(error) = &summary.error {
                text.push_str("\n错误: ");
                text.push_str(error);
            }
        }
        if let Some(result) = &self.result {
            text.push_str("\n\n");
            text.push_str(result);
        }
        sanitize_terminal_text(&text)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTurn {
    pub user_prompt: Option<String>,
    pub reasoning: String,
    pub text: String,
    pub warnings: Vec<String>,
    pub tools: Vec<ToolRun>,
    pub error: Option<String>,
    pub awaiting_save: bool,
    pub(crate) reasoning_sanitizer: TerminalTextSanitizer,
    pub(crate) text_sanitizer: TerminalTextSanitizer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionView {
    pub snapshot: Option<SessionSnapshot>,
    pub live: LiveTurn,
    pub scroll: u16,
    pub at_bottom: bool,
    pub unread: usize,
    pub selected_message: usize,
    pub selected_tool: Option<usize>,
    pub subagents: BTreeMap<String, SubagentInstanceSnapshot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextMeter {
    pub estimate: Option<ContextEstimate>,
    pub error: Option<String>,
    pub loading: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsState {
    pub snapshot: Option<SettingsSnapshot>,
    pub section: SettingsSection,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPreference {
    pub permissions: PermissionProfile,
    pub reasoning: ReasoningLevel,
}

#[derive(Debug)]
pub struct AppState {
    pub workspace: PathBuf,
    pub sessions: Vec<SessionInfo>,
    pub active_session_id: Option<String>,
    pub views: BTreeMap<String, SessionView>,
    pub models: Vec<crate::backend::ModelOption>,
    pub permissions: PermissionProfile,
    pub stored_permissions: PermissionProfile,
    pub session_preferences: BTreeMap<String, SessionPreference>,
    pub permission_override: Option<PermissionProfile>,
    pub reasoning: ReasoningLevel,
    pub reasoning_expanded: bool,
    pub sessions_visible: bool,
    pub inspector_visible: bool,
    pub page: MainPage,
    pub inspector_tab: InspectorTab,
    pub overlay: Option<Overlay>,
    pub approvals: VecDeque<PendingApproval>,
    pub approval_scroll: u16,
    pub subagent_transcripts: BTreeMap<String, crate::backend::SubagentTranscript>,
    pub composer: Composer,
    pub completion: Option<CompletionPopup>,
    pub context: ContextMeter,
    pub settings: SettingsState,
    pub selected_session: usize,
    pub session_search: String,
    pub session_search_active: bool,
    pub selected_inspector: usize,
    pub terminal_size: (u16, u16),
    pub status: Option<String>,
    pub should_quit: bool,
    pub cancelled_active_on_exit: bool,
    pub exit_when_idle: bool,
    pub no_color: bool,
    pub(crate) tick: u64,
    pub(crate) spinner: usize,
    pub(crate) request_id: u64,
    pub(crate) context_request: u64,
    pub(crate) settings_request: u64,
    pub(crate) path_request: u64,
    pub(crate) context_debounce: Option<u8>,
    pub(crate) path_debounce: Option<(u8, Range<usize>, String)>,
    pub(crate) ctrl_c_armed_until_tick: Option<u64>,
    pub(crate) render_cache: HashMap<(String, usize, u16, u64), ratatui::text::Text<'static>>,
}

#[derive(Debug)]
pub enum Message {
    Terminal(Event),
    Workspace(Result<WorkspaceEvent, BackendError>),
    Tick,
    SnapshotLoaded(Result<WorkspaceSnapshot, BackendError>),
    CommandFinished(Result<CommandResult, BackendError>),
    SettingsLoaded {
        request_id: u64,
        result: Result<SettingsSnapshot, BackendError>,
    },
    ContextEstimated {
        request_id: u64,
        result: Result<ContextEstimate, BackendError>,
    },
    PathsCompleted {
        request_id: u64,
        replace: Range<usize>,
        result: Result<Vec<PathCompletion>, BackendError>,
    },
    StatePersisted(Result<(), String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    RefreshSnapshot,
    Backend(BackendCommand),
    LoadSettings {
        request_id: u64,
    },
    EstimateContext {
        request_id: u64,
        session_id: String,
        draft: String,
        model: Option<ModelSelection>,
        permissions: PermissionProfile,
    },
    CompletePaths {
        request_id: u64,
        query: String,
        replace: Range<usize>,
    },
    Copy(String),
    PersistState,
}

impl AppState {
    pub(crate) fn bottom_panel(&self) -> BottomPanel {
        if !self.approvals.is_empty() {
            return BottomPanel::Approval;
        }
        if let Some(overlay) = &self.overlay {
            return match overlay {
                Overlay::Help => BottomPanel::Help,
                Overlay::ActionPalette { .. } => BottomPanel::ActionPalette,
                Overlay::ExitConfirm => BottomPanel::ExitConfirm,
                Overlay::ConfirmDelete(_) => BottomPanel::ConfirmDelete,
                Overlay::SettingsEditor(_) => BottomPanel::SettingsEditor,
                Overlay::SubagentFollowUp { .. } => BottomPanel::SubagentFollowUp,
            };
        }
        match self.page {
            MainPage::Chat => BottomPanel::Composer,
            MainPage::Sessions => BottomPanel::Sessions,
            MainPage::Inspector => BottomPanel::Inspector,
            MainPage::Settings => BottomPanel::Settings,
        }
    }

    pub fn new(
        workspace: PathBuf,
        snapshot: WorkspaceSnapshot,
        persisted: Option<WorkspaceTuiState>,
        permission_override: Option<PermissionProfile>,
        no_color: bool,
    ) -> Self {
        let persisted = persisted.unwrap_or_default();
        let active_session_id = snapshot
            .active_session
            .as_ref()
            .map(|session| session.info.id.clone())
            .or_else(|| persisted.recent_session.clone());
        let fallback_permissions = snapshot
            .active_session
            .as_ref()
            .map_or(persisted.permissions, |session| session.info.permissions);
        let stored_permissions = if persisted.recent_session.is_some() {
            persisted.permissions
        } else {
            fallback_permissions
        };
        let permissions = permission_override.unwrap_or_else(|| {
            if persisted.recent_session.is_some() {
                persisted.permissions
            } else {
                fallback_permissions
            }
        });
        let mut views = BTreeMap::new();
        let mut approvals = VecDeque::new();
        if let Some(active) = snapshot.active_session.clone() {
            let subagents = active
                .subagents
                .iter()
                .cloned()
                .map(|agent| (agent.id.clone(), agent))
                .collect();
            approvals.extend(
                active
                    .approvals
                    .iter()
                    .cloned()
                    .map(|request| PendingApproval {
                        session_id: active.info.id.clone(),
                        request,
                    }),
            );
            views.insert(
                active.info.id.clone(),
                SessionView {
                    snapshot: Some(active),
                    subagents,
                    at_bottom: true,
                    ..SessionView::default()
                },
            );
        }
        let no_models = snapshot.models.is_empty();
        let estimate_context_on_start = active_session_id.is_some() && !no_models;
        let mut session_preferences = BTreeMap::new();
        if let Some(session_id) = &active_session_id {
            session_preferences.insert(
                session_id.clone(),
                SessionPreference {
                    permissions,
                    reasoning: persisted.reasoning,
                },
            );
        }
        Self {
            workspace,
            sessions: snapshot.sessions,
            active_session_id,
            views,
            models: snapshot.models,
            permissions,
            stored_permissions,
            session_preferences,
            permission_override,
            reasoning: persisted.reasoning,
            reasoning_expanded: persisted.reasoning_expanded,
            sessions_visible: persisted.sessions_visible,
            inspector_visible: persisted.inspector_visible,
            page: MainPage::Chat,
            inspector_tab: InspectorTab::Run,
            overlay: None,
            approvals,
            approval_scroll: 0,
            subagent_transcripts: BTreeMap::new(),
            composer: Composer::default(),
            completion: None,
            context: ContextMeter::default(),
            settings: SettingsState {
                section: SettingsSection::Models,
                ..SettingsState::default()
            },
            selected_session: 0,
            session_search: String::new(),
            session_search_active: false,
            selected_inspector: 0,
            terminal_size: (80, 24),
            status: no_models.then(|| "尚未配置模型，请先在设置中添加模型供应商。".to_string()),
            should_quit: false,
            cancelled_active_on_exit: false,
            exit_when_idle: false,
            no_color,
            tick: 0,
            spinner: 0,
            request_id: 0,
            context_request: 0,
            settings_request: 0,
            path_request: 0,
            context_debounce: estimate_context_on_start.then_some(1),
            path_debounce: None,
            ctrl_c_armed_until_tick: None,
            render_cache: HashMap::new(),
        }
    }

    pub fn layout_mode(&self) -> LayoutMode {
        LayoutMode::for_size(self.terminal_size.0, self.terminal_size.1)
    }

    pub fn active_view(&self) -> Option<&SessionView> {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.views.get(id))
    }

    pub fn active_view_mut(&mut self) -> Option<&mut SessionView> {
        let id = self.active_session_id.clone()?;
        self.views.get_mut(&id)
    }

    pub fn active_info(&self) -> Option<&SessionInfo> {
        let id = self.active_session_id.as_ref()?;
        self.sessions.iter().find(|session| &session.id == id)
    }

    pub fn has_active_work(&self) -> bool {
        self.sessions.iter().any(|session| session.running)
            || self.views.values().any(|view| {
                view.subagents
                    .values()
                    .any(|agent| agent.status.is_active())
            })
    }

    pub fn persisted_state(&self) -> WorkspaceTuiState {
        WorkspaceTuiState {
            recent_session: self.active_session_id.clone(),
            permissions: self.stored_permissions,
            reasoning: self.reasoning,
            reasoning_expanded: self.reasoning_expanded,
            sessions_visible: self.sessions_visible,
            inspector_visible: self.inspector_visible,
        }
    }

    fn next_request(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1).max(1);
        self.request_id
    }

    fn schedule_context_estimate(&mut self, ticks: u8) {
        self.context_request = self.next_request();
        self.context_debounce = Some(ticks.max(1));
    }

    fn invalidate_path_completion(&mut self) {
        self.path_request = self.next_request();
        self.path_debounce = None;
    }

    pub fn update(&mut self, message: Message) -> Vec<Effect> {
        match message {
            Message::Terminal(event) => self.handle_terminal(event),
            Message::Workspace(event) => match event {
                Ok(event) => self.handle_workspace_event(event),
                Err(error) => {
                    self.status = Some(error.to_string());
                    Vec::new()
                }
            },
            Message::Tick => self.handle_tick(),
            Message::SnapshotLoaded(result) => match result {
                Ok(snapshot) => self.apply_snapshot(snapshot),
                Err(error) => {
                    self.status = Some(error.to_string());
                    Vec::new()
                }
            },
            Message::CommandFinished(result) => self.handle_command_result(result),
            Message::SettingsLoaded { request_id, result } => {
                if request_id != self.settings_request {
                    return Vec::new();
                }
                self.settings.loading = false;
                match result {
                    Ok(settings) => {
                        self.models.clone_from(&settings.models);
                        self.settings.snapshot = Some(settings);
                        self.settings.error = None;
                    }
                    Err(error) => self.settings.error = Some(error.to_string()),
                }
                Vec::new()
            }
            Message::ContextEstimated { request_id, result } => {
                if request_id != self.context_request {
                    return Vec::new();
                }
                self.context.loading = false;
                match result {
                    Ok(estimate) => {
                        self.context.estimate = Some(estimate);
                        self.context.error = None;
                    }
                    Err(error) => self.context.error = Some(error.to_string()),
                }
                Vec::new()
            }
            Message::PathsCompleted {
                request_id,
                replace,
                result,
            } => {
                if request_id != self.path_request {
                    return Vec::new();
                }
                match result {
                    Ok(paths) if !paths.is_empty() => {
                        self.completion = Some(CompletionPopup {
                            kind: CompletionKind::WorkspacePath,
                            replace,
                            items: paths
                                .into_iter()
                                .map(|path| CompletionItem {
                                    detail: if path.directory { "目录" } else { "文件" }
                                        .to_string(),
                                    replacement: path.path.clone(),
                                    label: path.path,
                                })
                                .collect(),
                            selected: 0,
                            request_id,
                        });
                    }
                    Ok(_) => self.completion = None,
                    Err(error) => self.status = Some(format!("路径补全失败: {error}")),
                }
                Vec::new()
            }
            Message::StatePersisted(result) => {
                if let Err(error) = result {
                    self.status = Some(format!("无法保存 TUI 状态: {error}"));
                }
                Vec::new()
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) -> Vec<Effect> {
        self.sessions = snapshot.sessions;
        self.models = snapshot.models;
        if let Some(active) = snapshot.active_session {
            self.active_session_id = Some(active.info.id.clone());
            self.reconcile_session(active);
        }
        if self.models.is_empty() {
            self.status = Some("尚未配置模型，请按 Ctrl+, 或输入 :settings。".to_string());
        } else if self.active_session_id.is_some() {
            self.schedule_context_estimate(1);
        }
        vec![Effect::PersistState]
    }

    fn reconcile_session(&mut self, snapshot: SessionSnapshot) {
        let id = snapshot.info.id.clone();
        if let Some(info) = self.sessions.iter_mut().find(|info| info.id == id) {
            *info = snapshot.info.clone();
        } else {
            self.sessions.push(snapshot.info.clone());
        }
        self.replace_approval_queue(&snapshot.info.id, snapshot.approvals.clone());
        let view = self.views.entry(id).or_default();
        let first_load = view.snapshot.is_none();
        view.subagents = snapshot
            .subagents
            .iter()
            .cloned()
            .map(|agent| (agent.id.clone(), agent))
            .collect();
        view.snapshot = Some(snapshot);
        view.live = LiveTurn::default();
        if first_load {
            view.at_bottom = true;
        }
        self.render_cache.clear();
    }

    fn handle_workspace_event(&mut self, event: WorkspaceEvent) -> Vec<Effect> {
        match event {
            WorkspaceEvent::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            WorkspaceEvent::SessionsChanged(sessions) => {
                self.sessions = sessions;
                let active_valid = self.active_session_id.as_ref().is_some_and(|id| {
                    self.sessions
                        .iter()
                        .any(|session| &session.id == id && !session.archived)
                });
                if active_valid {
                    return Vec::new();
                }
                if let Some(next_id) = self
                    .sessions
                    .iter()
                    .find(|session| !session.archived)
                    .map(|session| session.id.clone())
                {
                    self.active_session_id = Some(next_id.clone());
                    self.schedule_context_estimate(1);
                    return vec![Effect::Backend(BackendCommand::LoadSession {
                        session_id: next_id,
                    })];
                }
                self.active_session_id = None;
                self.context_request = self.next_request();
                self.context_debounce = None;
                self.context.loading = false;
                vec![Effect::Backend(BackendCommand::CreateSession)]
            }
            WorkspaceEvent::SessionLoaded(snapshot) => {
                let refresh_context = self.active_session_id.as_deref() == Some(&snapshot.info.id);
                self.reconcile_session(snapshot);
                if refresh_context {
                    self.schedule_context_estimate(1);
                }
                Vec::new()
            }
            WorkspaceEvent::SessionRunning {
                session_id,
                running,
            } => {
                if let Some(session) = self.sessions.iter_mut().find(|item| item.id == session_id) {
                    session.running = running;
                }
                self.maybe_finish_waiting()
            }
            WorkspaceEvent::ApprovalQueue {
                session_id,
                approvals,
            } => {
                self.replace_approval_queue(&session_id, approvals);
                Vec::new()
            }
            WorkspaceEvent::SubagentsChanged {
                session_id,
                subagents,
            } => {
                self.views.entry(session_id).or_default().subagents = subagents
                    .into_iter()
                    .map(|agent| (agent.id.clone(), agent))
                    .collect();
                self.maybe_finish_waiting()
            }
            WorkspaceEvent::Agent {
                session_id,
                origin,
                event,
            } => match origin {
                AgentEventOrigin::SubagentRun { instance_id, .. } => {
                    self.apply_subagent_agent_event(session_id, instance_id, event)
                }
                AgentEventOrigin::Session | AgentEventOrigin::ParentTurn { .. } => {
                    self.apply_agent_event(session_id, event)
                }
            },
            WorkspaceEvent::TurnSaved { session_id } => {
                vec![Effect::Backend(BackendCommand::LoadSession { session_id })]
            }
            WorkspaceEvent::SettingsChanged => self.request_settings(),
            WorkspaceEvent::BroadcastLagged => vec![Effect::RefreshSnapshot],
            WorkspaceEvent::Notice(message) => {
                self.status = Some(sanitize_terminal_text(&message));
                Vec::new()
            }
        }
    }

    fn apply_agent_event(&mut self, session_id: String, event: AgentEvent) -> Vec<Effect> {
        let is_active = self.active_session_id.as_deref() == Some(&session_id);
        let view = self.views.entry(session_id.clone()).or_default();
        match event {
            AgentEvent::TurnStarted => {
                view.live = LiveTurn::default();
                if let Some(info) = self.sessions.iter_mut().find(|item| item.id == session_id) {
                    info.running = true;
                }
            }
            AgentEvent::ModelCallStarted => {}
            AgentEvent::Warning(warning) => {
                view.live.warnings.push(sanitize_terminal_text(&warning));
            }
            AgentEvent::ReasoningDelta(delta) => {
                view.live
                    .reasoning_sanitizer
                    .push_to(&delta, &mut view.live.reasoning);
            }
            AgentEvent::TextDelta(delta) => {
                view.live
                    .text_sanitizer
                    .push_to(&delta, &mut view.live.text);
                if !is_active || !view.at_bottom {
                    view.unread = view.unread.saturating_add(1);
                }
            }
            AgentEvent::AgentMessage(message) => {
                view.live.text_sanitizer.reset();
                view.live.text.clear();
                view.live
                    .text_sanitizer
                    .push_to(&message, &mut view.live.text);
            }
            AgentEvent::ToolCallStarted { id, name } => view.live.tools.push(ToolRun {
                id,
                name,
                status: TurnStatus::Running,
                summary: None,
                result: None,
            }),
            AgentEvent::ToolCallFinished {
                id,
                name,
                ok,
                summary,
            } => {
                let position = view.live.tools.iter().position(|tool| tool.id == id);
                let index = position.unwrap_or_else(|| {
                    view.live.tools.push(ToolRun {
                        id: id.clone(),
                        name: name.clone(),
                        status: TurnStatus::Running,
                        summary: None,
                        result: None,
                    });
                    view.live.tools.len() - 1
                });
                let tool = view.live.tools.get_mut(index);
                if let Some(tool) = tool {
                    tool.status = if ok {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    };
                    tool.summary = summary.map(sanitize_tool_summary);
                }
            }
            AgentEvent::ApprovalRequested(request) => {
                let was_empty = self.approvals.is_empty();
                if !self.approvals.iter().any(|pending| {
                    pending.session_id == session_id && pending.request.id == request.id
                }) {
                    self.approvals.push_back(PendingApproval {
                        session_id,
                        request,
                    });
                    if was_empty {
                        self.approval_scroll = 0;
                    }
                }
            }
            AgentEvent::ApprovalResolved(decision) => {
                let resolved_head = self.approvals.front().is_some_and(|pending| {
                    pending.session_id == session_id && pending.request.id == decision.request_id
                });
                self.approvals.retain(|pending| {
                    pending.session_id != session_id || pending.request.id != decision.request_id
                });
                if resolved_head {
                    self.approval_scroll = 0;
                }
            }
            AgentEvent::SubagentUpdated(snapshot) => {
                view.subagents.insert(snapshot.id.clone(), *snapshot);
            }
            AgentEvent::SubagentStarted { id, .. } => {
                self.status = Some(format!("Subagent {id} 已启动"));
            }
            AgentEvent::SubagentFinished { id, ok, .. } => {
                self.status = Some(format!(
                    "Subagent {id} {}",
                    if ok { "已完成" } else { "失败" }
                ));
            }
            AgentEvent::TurnCompleted => {
                view.live.awaiting_save = true;
            }
            AgentEvent::Error(error) => {
                view.live.error = Some(sanitize_terminal_text(&error));
            }
        }
        self.maybe_finish_waiting()
    }

    fn apply_subagent_agent_event(
        &mut self,
        session_id: String,
        instance_id: String,
        event: AgentEvent,
    ) -> Vec<Effect> {
        match &event {
            AgentEvent::ApprovalRequested(request) => {
                let was_empty = self.approvals.is_empty();
                if !self.approvals.iter().any(|pending| {
                    pending.session_id == session_id && pending.request.id == request.id
                }) {
                    self.approvals.push_back(PendingApproval {
                        session_id: session_id.clone(),
                        request: request.clone(),
                    });
                    if was_empty {
                        self.approval_scroll = 0;
                    }
                }
            }
            AgentEvent::ApprovalResolved(decision) => {
                let resolved_head = self.approvals.front().is_some_and(|pending| {
                    pending.session_id == session_id && pending.request.id == decision.request_id
                });
                self.approvals.retain(|pending| {
                    pending.session_id != session_id || pending.request.id != decision.request_id
                });
                if resolved_head {
                    self.approval_scroll = 0;
                }
            }
            AgentEvent::SubagentUpdated(snapshot) => {
                self.views
                    .entry(session_id.clone())
                    .or_default()
                    .subagents
                    .insert(snapshot.id.clone(), (**snapshot).clone());
            }
            _ => {}
        }

        let instance = self
            .views
            .get(&session_id)
            .and_then(|view| view.subagents.get(&instance_id))
            .cloned();
        if let Some(instance) = instance {
            let line = serde_json::to_string(&event)
                .map(|line| sanitize_terminal_text(&line))
                .unwrap_or_else(|error| format!("无法显示 Subagent event: {error}"));
            let transcript = self
                .subagent_transcripts
                .entry(instance_id)
                .or_insert_with(|| crate::backend::SubagentTranscript {
                    instance: instance.clone(),
                    lines: Vec::new(),
                });
            transcript.instance = instance;
            transcript.lines.push(line);
            if transcript.lines.len() > 1_000 {
                let remove = transcript.lines.len() - 1_000;
                transcript.lines.drain(..remove);
            }
        }
        self.maybe_finish_waiting()
    }

    fn handle_command_result(
        &mut self,
        result: Result<CommandResult, BackendError>,
    ) -> Vec<Effect> {
        match result {
            Ok(CommandResult::Ack) => Vec::new(),
            Ok(CommandResult::Session(snapshot)) => {
                let refresh_context = self.active_session_id.as_deref() == Some(&snapshot.info.id);
                self.reconcile_session(snapshot);
                if refresh_context {
                    self.schedule_context_estimate(1);
                }
                Vec::new()
            }
            Ok(CommandResult::SessionCreated(snapshot)) => {
                self.active_session_id = Some(snapshot.info.id.clone());
                self.reconcile_session(snapshot);
                self.page = MainPage::Chat;
                self.schedule_context_estimate(1);
                vec![Effect::PersistState]
            }
            Ok(CommandResult::Settings(settings)) => {
                let had_no_models = self.models.is_empty();
                self.models.clone_from(&settings.models);
                self.settings.snapshot = Some(settings);
                self.settings.loading = false;
                if had_no_models && !self.models.is_empty() {
                    self.status = Some("模型设置已就绪，可返回聊天开始会话。".to_string());
                }
                Vec::new()
            }
            Ok(CommandResult::SubagentTranscript(transcript)) => {
                self.subagent_transcripts
                    .insert(transcript.instance.id.clone(), transcript.clone());
                self.status = Some(format!(
                    "已加载 {} 的 {} 行 transcript",
                    transcript.instance.identity.name,
                    transcript.lines.len()
                ));
                Vec::new()
            }
            Ok(CommandResult::Notice(notice)) => {
                self.status = Some(sanitize_terminal_text(&notice));
                Vec::new()
            }
            Err(error) => {
                self.status = Some(error.to_string());
                vec![Effect::RefreshSnapshot]
            }
        }
    }

    fn request_settings(&mut self) -> Vec<Effect> {
        let request_id = self.next_request();
        self.settings_request = request_id;
        self.settings.loading = true;
        vec![Effect::LoadSettings { request_id }]
    }

    fn maybe_finish_waiting(&mut self) -> Vec<Effect> {
        if self.exit_when_idle && !self.has_active_work() {
            self.should_quit = true;
            return vec![Effect::PersistState];
        }
        Vec::new()
    }

    fn approval_head_key(&self) -> Option<(String, String)> {
        self.approvals
            .front()
            .map(|pending| (pending.session_id.clone(), pending.request.id.clone()))
    }

    fn replace_approval_queue(&mut self, session_id: &str, approvals: Vec<ApprovalRequest>) {
        let previous_head = self.approval_head_key();
        let incoming = approvals
            .iter()
            .map(|request| request.id.clone())
            .collect::<HashSet<_>>();
        self.approvals.retain(|pending| {
            pending.session_id != session_id || incoming.contains(&pending.request.id)
        });
        let existing = self
            .approvals
            .iter()
            .filter(|pending| pending.session_id == session_id)
            .map(|pending| pending.request.id.clone())
            .collect::<HashSet<_>>();
        self.approvals.extend(
            approvals
                .into_iter()
                .filter(|request| !existing.contains(&request.id))
                .map(|request| PendingApproval {
                    session_id: session_id.to_string(),
                    request,
                }),
        );
        if self.approval_head_key() != previous_head {
            self.approval_scroll = 0;
        }
    }

    fn handle_tick(&mut self) -> Vec<Effect> {
        self.tick = self.tick.wrapping_add(1);
        self.spinner = (self.spinner + 1) % 10;
        let mut effects = Vec::new();

        if self
            .ctrl_c_armed_until_tick
            .is_some_and(|deadline| self.tick > deadline)
        {
            self.ctrl_c_armed_until_tick = None;
            if self.status.as_deref() == Some(CTRL_C_FORCE_EXIT_HINT) {
                self.status = None;
            }
        }

        if let Some(ticks) = self.context_debounce {
            if ticks > 1 {
                self.context_debounce = Some(ticks - 1);
            } else {
                self.context_debounce = None;
                if let Some(session_id) = self.active_session_id.clone() {
                    let request_id = self.next_request();
                    self.context_request = request_id;
                    self.context.loading = true;
                    effects.push(Effect::EstimateContext {
                        request_id,
                        session_id,
                        draft: self.composer.text().to_string(),
                        model: self.current_model(),
                        permissions: self.permissions,
                    });
                }
            }
        }

        if let Some((ticks, range, query)) = self.path_debounce.clone() {
            if ticks > 1 {
                self.path_debounce = Some((ticks - 1, range, query));
            } else {
                self.path_debounce = None;
                let request_id = self.next_request();
                self.path_request = request_id;
                effects.push(Effect::CompletePaths {
                    request_id,
                    query,
                    replace: range,
                });
            }
        }
        effects.extend(self.maybe_finish_waiting());
        effects
    }

    fn handle_terminal(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Resize(width, height) => {
                self.terminal_size = (width, height);
                Vec::new()
            }
            Event::Paste(value) => {
                if let Some(Overlay::SettingsEditor(editor)) = &mut self.overlay {
                    if let Some(field) = editor.fields.get_mut(editor.selected) {
                        field.value.push_str(&sanitize_input(&value));
                    }
                    return Vec::new();
                }
                if let Some(Overlay::SubagentFollowUp { value: draft, .. }) = &mut self.overlay {
                    draft.push_str(&sanitize_input(&value));
                    return Vec::new();
                }
                if matches!(self.page, MainPage::Chat) && self.overlay.is_none() {
                    self.composer.insert_str(&value);
                    return self.draft_changed();
                }
                Vec::new()
            }
            Event::Key(key)
                if key.kind == KeyEventKind::Repeat
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c') =>
            {
                Vec::new()
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            _ => Vec::new(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.handle_ctrl_c();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.overlay = Some(Overlay::ExitConfirm);
            return Vec::new();
        }
        if !self.approvals.is_empty() {
            return self.handle_approval_key(key);
        }
        if self.overlay.is_some() {
            return self.handle_overlay_key(key);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => unreachable!(),
                KeyCode::Char('b') => {
                    self.page = if self.page == MainPage::Sessions {
                        MainPage::Chat
                    } else {
                        MainPage::Sessions
                    };
                    return vec![Effect::PersistState];
                }
                KeyCode::Char('g') => {
                    self.page = if self.page == MainPage::Inspector {
                        MainPage::Chat
                    } else {
                        MainPage::Inspector
                    };
                    return vec![Effect::PersistState];
                }
                KeyCode::Char(',') => return self.open_settings(),
                KeyCode::Char('p') => {
                    self.overlay = Some(Overlay::ActionPalette { selected: 0 });
                    return Vec::new();
                }
                KeyCode::Char('o') => {
                    self.reasoning_expanded = !self.reasoning_expanded;
                    return vec![Effect::PersistState];
                }
                KeyCode::Char('j') if self.page == MainPage::Chat => {
                    self.composer.insert_newline();
                    return self.draft_changed();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::F(2) => return self.cycle_model(),
            KeyCode::F(3) => return self.cycle_reasoning(),
            KeyCode::F(4) => return self.cycle_permissions(),
            _ => {}
        }
        if key.code == KeyCode::F(1) {
            self.overlay = Some(Overlay::Help);
            return Vec::new();
        }

        match self.page {
            MainPage::Chat => self.handle_chat_key(key),
            MainPage::Sessions => self.handle_sessions_key(key),
            MainPage::Inspector => self.handle_inspector_key(key),
            MainPage::Settings => self.handle_settings_key(key),
        }
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let approved = match key.code {
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'y') => true,
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'n') => false,
            KeyCode::Up => {
                self.approval_scroll = self.approval_scroll.saturating_sub(1);
                return Vec::new();
            }
            KeyCode::Down => {
                self.approval_scroll = self.approval_scroll.saturating_add(1);
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.approval_scroll = self.approval_scroll.saturating_sub(10);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.approval_scroll = self.approval_scroll.saturating_add(10);
                return Vec::new();
            }
            KeyCode::Home => {
                self.approval_scroll = 0;
                return Vec::new();
            }
            KeyCode::End => {
                self.approval_scroll = u16::MAX;
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        let Some(pending) = self.approvals.pop_front() else {
            return Vec::new();
        };
        self.approval_scroll = 0;
        vec![Effect::Backend(BackendCommand::ResolveApproval {
            session_id: pending.session_id,
            decision: if approved {
                ApprovalDecision::approve(pending.request.id)
            } else {
                ApprovalDecision::deny(pending.request.id)
            },
        })]
    }

    fn cancel_or_clear(&mut self) -> Vec<Effect> {
        if let Some(pending) = self.approvals.front() {
            match &pending.request.origin {
                ApprovalOrigin::ParentTurn { .. } => {
                    return vec![Effect::Backend(BackendCommand::CancelTurn {
                        session_id: pending.session_id.clone(),
                    })];
                }
                ApprovalOrigin::SubagentRun { instance_id, .. } => {
                    return vec![Effect::Backend(BackendCommand::CancelSubagent {
                        session_id: pending.session_id.clone(),
                        instance_id: instance_id.clone(),
                    })];
                }
                ApprovalOrigin::Unknown => {
                    return vec![Effect::Backend(BackendCommand::CancelTurn {
                        session_id: pending.session_id.clone(),
                    })];
                }
            }
        }
        let Some(session_id) = self.active_session_id.clone() else {
            self.composer.clear();
            return self.draft_changed();
        };
        let running = self
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.running);
        if !running {
            self.composer.clear();
            return self.draft_changed();
        }

        vec![Effect::Backend(BackendCommand::CancelTurn { session_id })]
    }

    fn handle_ctrl_c(&mut self) -> Vec<Effect> {
        if self
            .ctrl_c_armed_until_tick
            .is_some_and(|deadline| self.tick <= deadline)
        {
            return self.force_exit();
        }

        self.ctrl_c_armed_until_tick = Some(self.tick.saturating_add(CTRL_C_FORCE_EXIT_TICKS));
        let effects = self.cancel_or_clear();
        self.status = Some(CTRL_C_FORCE_EXIT_HINT.to_string());
        effects
    }

    fn force_exit(&mut self) -> Vec<Effect> {
        let had_active_work = self.has_active_work();
        let running = self
            .sessions
            .iter()
            .filter(|session| session.running)
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let mut effects = running
            .into_iter()
            .map(|session_id| Effect::Backend(BackendCommand::CancelTurn { session_id }))
            .collect::<Vec<_>>();
        for (session_id, view) in &self.views {
            for subagent in view
                .subagents
                .values()
                .filter(|agent| agent.status.is_active())
            {
                effects.push(Effect::Backend(BackendCommand::CancelSubagent {
                    session_id: session_id.clone(),
                    instance_id: subagent.id.clone(),
                }));
            }
        }
        self.ctrl_c_armed_until_tick = None;
        self.exit_when_idle = false;
        self.cancelled_active_on_exit = had_active_work;
        self.should_quit = true;
        effects.push(Effect::PersistState);
        effects
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(overlay) = self.overlay.take() else {
            return Vec::new();
        };
        match overlay {
            Overlay::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
                    self.overlay = Some(Overlay::Help);
                }
                Vec::new()
            }
            Overlay::ActionPalette { mut selected } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.overlay = Some(Overlay::ActionPalette { selected });
                    Vec::new()
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(7);
                    self.overlay = Some(Overlay::ActionPalette { selected });
                    Vec::new()
                }
                KeyCode::Enter => self.run_palette_action(selected),
                _ => {
                    self.overlay = Some(Overlay::ActionPalette { selected });
                    Vec::new()
                }
            },
            Overlay::ExitConfirm => self.handle_exit_key(key),
            Overlay::ConfirmDelete(target) => match key.code {
                KeyCode::Char(character) if character.eq_ignore_ascii_case(&'y') => {
                    let command = match target {
                        DeleteTarget::SubagentInstance {
                            session_id,
                            instance_id,
                        } => BackendCommand::DeleteSubagent {
                            session_id,
                            instance_id,
                        },
                        target => BackendCommand::Settings(delete_command(target)),
                    };
                    vec![Effect::Backend(command)]
                }
                KeyCode::Char(character) if character.eq_ignore_ascii_case(&'n') => Vec::new(),
                KeyCode::Esc => Vec::new(),
                _ => {
                    self.overlay = Some(Overlay::ConfirmDelete(target));
                    Vec::new()
                }
            },
            Overlay::SettingsEditor(mut editor) => {
                if key.code == KeyCode::Esc {
                    return Vec::new();
                }
                let keep_open = key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('t')
                    && matches!(editor.kind, EditorKind::McpServer { .. });
                let result = self.handle_editor_key(&mut editor, key);
                if (result.is_empty() || keep_open) && self.overlay.is_none() {
                    self.overlay = Some(Overlay::SettingsEditor(editor));
                }
                result
            }
            Overlay::SubagentFollowUp {
                instance_id,
                mut value,
            } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Enter if !value.trim().is_empty() => {
                    let Some(session_id) = self.active_session_id.clone() else {
                        return Vec::new();
                    };
                    vec![Effect::Backend(BackendCommand::FollowUpSubagent {
                        session_id,
                        instance_id,
                        prompt: value.trim().to_string(),
                    })]
                }
                KeyCode::Backspace => {
                    value.pop();
                    self.overlay = Some(Overlay::SubagentFollowUp { instance_id, value });
                    Vec::new()
                }
                KeyCode::Char(character) if !character.is_control() => {
                    value.push(character);
                    self.overlay = Some(Overlay::SubagentFollowUp { instance_id, value });
                    Vec::new()
                }
                _ => {
                    self.overlay = Some(Overlay::SubagentFollowUp { instance_id, value });
                    Vec::new()
                }
            },
        }
    }

    fn handle_exit_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => Vec::new(),
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'r') => Vec::new(),
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'w') && self.has_active_work() =>
            {
                self.exit_when_idle = true;
                self.status = Some("等待活动任务完成后退出…".to_string());
                Vec::new()
            }
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'x') && self.has_active_work() =>
            {
                self.force_exit()
            }
            KeyCode::Enter if !self.has_active_work() => {
                self.should_quit = true;
                vec![Effect::PersistState]
            }
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'q') && !self.has_active_work() =>
            {
                self.should_quit = true;
                vec![Effect::PersistState]
            }
            _ => {
                self.overlay = Some(Overlay::ExitConfirm);
                Vec::new()
            }
        }
    }

    fn run_palette_action(&mut self, selected: usize) -> Vec<Effect> {
        match selected {
            0 => self.open_settings(),
            1 => {
                self.page = MainPage::Sessions;
                Vec::new()
            }
            2 => self.cycle_model(),
            3 => self.cycle_reasoning(),
            4 => self.cycle_permissions(),
            5 => self.compact_active(),
            6 => self.reset_active(),
            _ => {
                self.overlay = Some(Overlay::ExitConfirm);
                Vec::new()
            }
        }
    }

    fn handle_chat_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::ALT) {
            let turn_count = self
                .active_view()
                .and_then(|view| view.snapshot.as_ref())
                .map_or(0, |snapshot| snapshot.session.turns.len());
            if turn_count > 0 {
                match key.code {
                    KeyCode::Up => {
                        if let Some(view) = self.active_view_mut() {
                            view.selected_message = view.selected_message.saturating_sub(1);
                            view.selected_tool = None;
                        }
                        return Vec::new();
                    }
                    KeyCode::Down => {
                        if let Some(view) = self.active_view_mut() {
                            view.selected_message =
                                (view.selected_message + 1).min(turn_count.saturating_sub(1));
                            view.selected_tool = None;
                        }
                        return Vec::new();
                    }
                    _ => {}
                }
            }
        }
        if let Some(completion) = &mut self.completion {
            match key.code {
                KeyCode::Up => {
                    completion.selected = completion.selected.saturating_sub(1);
                    return Vec::new();
                }
                KeyCode::Down => {
                    completion.selected =
                        (completion.selected + 1).min(completion.items.len().saturating_sub(1));
                    return Vec::new();
                }
                KeyCode::Tab | KeyCode::Enter => return self.accept_completion(),
                KeyCode::Esc => {
                    self.completion = None;
                    return Vec::new();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                self.composer.insert_newline();
                self.draft_changed()
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Backspace => {
                self.composer.backspace();
                self.draft_changed()
            }
            KeyCode::Delete => {
                self.composer.delete();
                self.draft_changed()
            }
            KeyCode::Left => {
                self.composer.move_left();
                Vec::new()
            }
            KeyCode::Right => {
                self.composer.move_right();
                Vec::new()
            }
            KeyCode::Home => {
                self.composer.move_home();
                Vec::new()
            }
            KeyCode::End => {
                self.composer.move_end();
                Vec::new()
            }
            KeyCode::Up if self.composer.text().contains('\n') => {
                self.composer.move_vertical(-1);
                Vec::new()
            }
            KeyCode::Down if self.composer.text().contains('\n') => {
                self.composer.move_vertical(1);
                Vec::new()
            }
            KeyCode::Up => {
                self.composer.history_previous();
                self.draft_changed()
            }
            KeyCode::Down => {
                self.composer.history_next();
                self.draft_changed()
            }
            KeyCode::PageUp => {
                if let Some(view) = self.active_view_mut() {
                    view.scroll = view.scroll.saturating_sub(10);
                    view.at_bottom = false;
                }
                Vec::new()
            }
            KeyCode::PageDown => {
                if let Some(view) = self.active_view_mut() {
                    view.scroll = view.scroll.saturating_add(10);
                }
                Vec::new()
            }
            KeyCode::Char('Y') => {
                if let Some(text) = self
                    .selected_tool_text()
                    .or_else(|| self.selected_message_text())
                {
                    vec![Effect::Copy(text)]
                } else {
                    self.composer.insert_char('Y');
                    self.draft_changed()
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_char(character);
                self.draft_changed()
            }
            _ => Vec::new(),
        }
    }

    fn submit_composer(&mut self) -> Vec<Effect> {
        let prompt = self.composer.text().trim().to_string();
        if prompt.is_empty() {
            return Vec::new();
        }
        if prompt.starts_with(':') {
            let _ = self.composer.submit();
            self.completion = None;
            self.schedule_context_estimate(1);
            self.invalidate_path_completion();
            return self.run_local_command(&prompt);
        }
        if self.models.is_empty() {
            self.page = MainPage::Settings;
            self.settings.section = SettingsSection::Models;
            self.status = Some("请先配置模型。".to_string());
            return self.request_settings();
        }
        let Some(session_id) = self.active_session_id.clone() else {
            self.status = Some("请先创建会话。".to_string());
            self.page = MainPage::Sessions;
            return Vec::new();
        };
        if self
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.running)
        {
            self.status = Some("当前会话仍在运行，不能重复提交。".to_string());
            return Vec::new();
        }
        let prompt = self.composer.submit().expect("validated non-empty prompt");
        self.completion = None;
        self.schedule_context_estimate(1);
        self.invalidate_path_completion();
        if let Some(info) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            info.running = true;
        }
        let view = self.views.entry(session_id.clone()).or_default();
        view.live = LiveTurn {
            user_prompt: Some(prompt.clone()),
            ..LiveTurn::default()
        };
        view.at_bottom = true;
        vec![Effect::Backend(BackendCommand::StartTurn {
            session_id,
            prompt,
            model: self.current_model(),
            permissions: self.permissions,
        })]
    }

    fn run_local_command(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().next().unwrap_or_default() {
            ":settings" => self.open_settings(),
            ":sessions" => {
                self.page = MainPage::Sessions;
                Vec::new()
            }
            ":compact" => self.compact_active(),
            ":reset" => self.reset_active(),
            ":quit" => {
                self.overlay = Some(Overlay::ExitConfirm);
                Vec::new()
            }
            _ => {
                self.status = Some(format!("未知本地动作: {command}"));
                Vec::new()
            }
        }
    }

    fn draft_changed(&mut self) -> Vec<Effect> {
        self.schedule_context_estimate(9);
        self.completion = None;
        self.invalidate_path_completion();
        let text = self.composer.text();
        if text.starts_with('/') && !text.starts_with("//") && !text.contains(char::is_whitespace) {
            let token = text.trim_start_matches('/').to_lowercase();
            let commands = self
                .settings
                .snapshot
                .as_ref()
                .map(|settings| settings.commands.as_slice())
                .unwrap_or_default();
            let items = commands
                .iter()
                .filter(|command| command.name.to_lowercase().starts_with(&token))
                .map(|command| CompletionItem {
                    label: format!("/{}", command.name),
                    replacement: format!("/{}", command.name),
                    detail: command.description.clone(),
                })
                .collect::<Vec<_>>();
            if !items.is_empty() {
                self.completion = Some(CompletionPopup {
                    kind: CompletionKind::ManagedCommand,
                    replace: 0..text.len(),
                    items,
                    selected: 0,
                    request_id: 0,
                });
            }
        } else if let Some((range, query)) = path_token(text, self.composer.cursor()) {
            self.path_debounce = Some((3, range, query));
        }
        Vec::new()
    }

    fn accept_completion(&mut self) -> Vec<Effect> {
        let Some(completion) = self.completion.take() else {
            return Vec::new();
        };
        let Some(item) = completion.items.get(completion.selected) else {
            return Vec::new();
        };
        self.composer.replace(completion.replace, &item.replacement);
        self.schedule_context_estimate(9);
        self.invalidate_path_completion();
        Vec::new()
    }

    fn selected_message_text(&self) -> Option<String> {
        let view = self.active_view()?;
        let snapshot = view.snapshot.as_ref()?;
        let record = snapshot.session.turns.get(view.selected_message)?;
        let mut text = String::new();
        if let Some(content) = &record.turn.user_message.content {
            text.push_str(content);
        }
        if let Some(assistant) = &record.turn.assistant_message
            && let Some(content) = &assistant.content
        {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(content);
        }
        Some(sanitize_terminal_text(&text))
    }

    fn selected_tool_text(&self) -> Option<String> {
        let view = self.active_view()?;
        let index = view.selected_tool?;
        let tool = view.live.tools.get(index)?;
        Some(tool.plain_text())
    }

    fn current_model(&self) -> Option<ModelSelection> {
        self.active_info()
            .and_then(|session| session.model.clone())
            .map(|mut model| {
                model.reasoning = self.reasoning;
                model
            })
    }

    fn compact_active(&mut self) -> Vec<Effect> {
        self.active_session_id
            .clone()
            .map_or_else(Vec::new, |session_id| {
                vec![Effect::Backend(BackendCommand::CompactSession {
                    session_id,
                })]
            })
    }

    fn reset_active(&mut self) -> Vec<Effect> {
        self.active_session_id
            .clone()
            .map_or_else(Vec::new, |session_id| {
                vec![Effect::Backend(BackendCommand::ResetSession { session_id })]
            })
    }

    fn open_settings(&mut self) -> Vec<Effect> {
        self.page = MainPage::Settings;
        self.request_settings()
    }

    fn filtered_session_indices(&self) -> Vec<usize> {
        let query = self.session_search.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                query.is_empty()
                    || session.id.to_lowercase().contains(&query)
                    || session.title.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn handle_sessions_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.session_search_active {
            match key.code {
                KeyCode::Esc => {
                    self.session_search_active = false;
                    return Vec::new();
                }
                KeyCode::Backspace => {
                    self.session_search.pop();
                    self.selected_session = 0;
                    return Vec::new();
                }
                KeyCode::Enter => self.session_search_active = false,
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !character.is_control() =>
                {
                    self.session_search.push(character);
                    self.selected_session = 0;
                    return Vec::new();
                }
                _ => return Vec::new(),
            }
        }
        let indices = self.filtered_session_indices();
        match key.code {
            KeyCode::Esc => {
                self.page = MainPage::Chat;
                Vec::new()
            }
            KeyCode::Char('/') => {
                self.session_search_active = true;
                self.session_search.clear();
                self.selected_session = 0;
                Vec::new()
            }
            KeyCode::Up => {
                self.selected_session = self.selected_session.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down => {
                self.selected_session =
                    (self.selected_session + 1).min(indices.len().saturating_sub(1));
                Vec::new()
            }
            KeyCode::Char('n') => vec![Effect::Backend(BackendCommand::CreateSession)],
            KeyCode::Enter => {
                let Some(index) = indices.get(self.selected_session).copied() else {
                    return Vec::new();
                };
                let info = self.sessions[index].clone();
                self.active_session_id = Some(info.id.clone());
                let preferences =
                    self.session_preferences
                        .entry(info.id.clone())
                        .or_insert(SessionPreference {
                            permissions: self
                                .permission_override
                                .unwrap_or(self.stored_permissions),
                            reasoning: info
                                .model
                                .as_ref()
                                .map_or(self.reasoning, |model| model.reasoning),
                        });
                self.permissions = preferences.permissions;
                self.reasoning = preferences.reasoning;
                self.page = MainPage::Chat;
                self.schedule_context_estimate(1);
                if let Some(view) = self.views.get_mut(&info.id) {
                    view.at_bottom = true;
                    view.unread = 0;
                    vec![Effect::PersistState]
                } else {
                    vec![
                        Effect::Backend(BackendCommand::LoadSession {
                            session_id: info.id,
                        }),
                        Effect::PersistState,
                    ]
                }
            }
            KeyCode::Char('a') => indices
                .get(self.selected_session)
                .map(|index| {
                    vec![Effect::Backend(BackendCommand::ArchiveSession {
                        session_id: self.sessions[*index].id.clone(),
                    })]
                })
                .unwrap_or_default(),
            KeyCode::Char('u') => indices
                .get(self.selected_session)
                .map(|index| {
                    vec![Effect::Backend(BackendCommand::RestoreSession {
                        session_id: self.sessions[*index].id.clone(),
                    })]
                })
                .unwrap_or_default(),
            KeyCode::Char('r') => indices
                .get(self.selected_session)
                .map(|index| {
                    vec![Effect::Backend(BackendCommand::ResetSession {
                        session_id: self.sessions[*index].id.clone(),
                    })]
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn handle_inspector_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => {
                self.page = MainPage::Chat;
                return Vec::new();
            }
            KeyCode::Tab => {
                self.inspector_tab = match self.inspector_tab {
                    InspectorTab::Run => InspectorTab::Subagents,
                    InspectorTab::Subagents => InspectorTab::Run,
                };
                self.selected_inspector = 0;
                let selected_tool = (self.inspector_tab == InspectorTab::Run).then_some(0);
                if let Some(view) = self.active_view_mut() {
                    view.selected_tool = selected_tool;
                }
                return Vec::new();
            }
            KeyCode::Up => {
                self.selected_inspector = self.selected_inspector.saturating_sub(1);
                let selected_tool = self.selected_inspector;
                if self.inspector_tab == InspectorTab::Run
                    && let Some(view) = self.active_view_mut()
                {
                    view.selected_tool = Some(selected_tool);
                }
                return Vec::new();
            }
            KeyCode::Down => {
                self.selected_inspector = self.selected_inspector.saturating_add(1);
                let selected_tool = self.selected_inspector;
                if self.inspector_tab == InspectorTab::Run
                    && let Some(view) = self.active_view_mut()
                {
                    view.selected_tool = Some(selected_tool);
                }
                return Vec::new();
            }
            _ => {}
        }

        if self.inspector_tab == InspectorTab::Run {
            if key.code == KeyCode::Char('Y') {
                let selected_tool = self.selected_inspector;
                if let Some(view) = self.active_view_mut() {
                    view.selected_tool = Some(selected_tool);
                }
                if let Some(text) = self.selected_tool_text() {
                    return vec![Effect::Copy(text)];
                }
            }
            return Vec::new();
        }

        let Some((session_id, instance_id)) = self.selected_subagent() else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Enter => vec![Effect::Backend(BackendCommand::LoadSubagentTranscript {
                session_id,
                instance_id,
            })],
            KeyCode::Char('f') => {
                self.overlay = Some(Overlay::SubagentFollowUp {
                    instance_id,
                    value: String::new(),
                });
                Vec::new()
            }
            KeyCode::Char('x') => vec![Effect::Backend(BackendCommand::CancelSubagent {
                session_id,
                instance_id,
            })],
            KeyCode::Char('d') => {
                self.overlay = Some(Overlay::ConfirmDelete(DeleteTarget::SubagentInstance {
                    session_id,
                    instance_id,
                }));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn selected_subagent(&self) -> Option<(String, String)> {
        let session_id = self.active_session_id.clone()?;
        let view = self.views.get(&session_id)?;
        let instance_id = view.subagents.keys().nth(self.selected_inspector)?.clone();
        Some((session_id, instance_id))
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => {
                self.page = MainPage::Chat;
                return Vec::new();
            }
            KeyCode::Left => {
                self.settings.section = self.settings.section.previous();
                self.settings.selected = 0;
                return Vec::new();
            }
            KeyCode::Right | KeyCode::Tab => {
                self.settings.section = self.settings.section.next();
                self.settings.selected = 0;
                return Vec::new();
            }
            KeyCode::Char('1') => self.settings.section = SettingsSection::Models,
            KeyCode::Char('2') => self.settings.section = SettingsSection::Mcp,
            KeyCode::Char('3') => self.settings.section = SettingsSection::Commands,
            KeyCode::Char('4') => self.settings.section = SettingsSection::Subagents,
            KeyCode::Up => {
                self.settings.selected = self.settings.selected.saturating_sub(1);
                return Vec::new();
            }
            KeyCode::Down => {
                self.settings.selected =
                    (self.settings.selected + 1).min(self.settings_item_count().saturating_sub(1));
                return Vec::new();
            }
            KeyCode::Char('r') => return self.request_settings(),
            KeyCode::Char('n') => {
                if let Some(editor) = self.new_settings_editor() {
                    self.overlay = Some(Overlay::SettingsEditor(editor));
                }
                return Vec::new();
            }
            KeyCode::Char('e') => {
                if self.selected_setting_is_read_only() {
                    self.status = Some("该设置来自 morrow.toml，只读且无法编辑。".to_string());
                    return Vec::new();
                }
                if let Some(editor) = self.edit_selected_setting() {
                    self.overlay = Some(Overlay::SettingsEditor(editor));
                }
                return Vec::new();
            }
            KeyCode::Char('i') if self.settings.section == SettingsSection::Mcp => {
                self.overlay = Some(Overlay::SettingsEditor(SettingsEditor {
                    title: "导入 MCP 配置".to_string(),
                    kind: EditorKind::McpImport,
                    fields: vec![field("JSON", "", false)],
                    selected: 0,
                }));
                return Vec::new();
            }
            KeyCode::Char('d') => {
                if self.selected_setting_is_read_only() {
                    self.status = Some("该设置来自 morrow.toml，只读且无法删除。".to_string());
                    return Vec::new();
                }
                if let Some(target) = self.selected_delete_target() {
                    self.overlay = Some(Overlay::ConfirmDelete(target));
                }
                return Vec::new();
            }
            KeyCode::Char('R') if self.settings.section == SettingsSection::Subagents => {
                self.overlay = Some(Overlay::ConfirmDelete(DeleteTarget::ResetSubagentRoles));
                return Vec::new();
            }
            KeyCode::Char('P') if self.settings.section == SettingsSection::Subagents => {
                self.overlay = Some(Overlay::ConfirmDelete(DeleteTarget::ResetSubagentProfiles));
                return Vec::new();
            }
            KeyCode::Char('f') => {
                if self.selected_setting_is_read_only() {
                    self.status = Some("只读设置无法从 TUI 同步或测试。".to_string());
                    return Vec::new();
                }
                return self.test_or_discover_selected();
            }
            KeyCode::Char(' ') => {
                if self.selected_setting_is_read_only() {
                    self.status = Some("只读设置无法启停。".to_string());
                    return Vec::new();
                }
                return self.toggle_selected_setting();
            }
            KeyCode::Enter => return self.activate_selected_setting(),
            _ => return Vec::new(),
        }
        self.settings.selected = 0;
        Vec::new()
    }

    fn new_settings_editor(&self) -> Option<SettingsEditor> {
        match self.settings.section {
            SettingsSection::Models => Some(SettingsEditor {
                title: "新建模型供应商".to_string(),
                kind: EditorKind::ModelProvider { original_id: None },
                fields: vec![
                    field("名称", "", false),
                    field("Base URL", "", false),
                    field("API Key（空值保留）", "", true),
                    field("启用 (yes/no)", "yes", false),
                    field("请求超时秒", "120", false),
                    field("模型 JSON（空数组将自动发现）", "[]", false),
                    field("设为默认模型 ID（空=不修改）", "", false),
                    field("默认推理 (off/high/max)", "off", false),
                ],
                selected: 0,
            }),
            SettingsSection::Mcp => Some(SettingsEditor {
                title: "新建 MCP Server".to_string(),
                kind: EditorKind::McpServer {
                    original_name: None,
                },
                fields: vec![
                    field("名称", "", false),
                    field("传输 (stdio/http)", "stdio", false),
                    field("命令", "", false),
                    field("参数 JSON", "[]", false),
                    field("工作目录", "", false),
                    field("URL", "", false),
                    field("Env keys（每行一个）", "", false),
                    field("Env secret values（逐行对应，空值保留）", "", true),
                    field("Header keys（每行一个）", "", false),
                    field("Header secret values（逐行对应，空值保留）", "", true),
                    field("启用 (yes/no)", "yes", false),
                    field("启动超时秒", "10", false),
                    field("工具超时秒", "60", false),
                ],
                selected: 0,
            }),
            SettingsSection::Commands => Some(SettingsEditor {
                title: "新建托管命令".to_string(),
                kind: EditorKind::ManagedCommand {
                    original_name: None,
                },
                fields: vec![
                    field("名称", "", false),
                    field("说明", "", false),
                    field("参数提示", "", false),
                    field("Prompt", "", false),
                ],
                selected: 0,
            }),
            SettingsSection::Subagents => Some(SettingsEditor {
                title: "新建 Subagent 身份".to_string(),
                kind: EditorKind::SubagentIdentity {
                    original_id: None,
                    avatar_configured: false,
                },
                fields: vec![
                    field("名称", "", false),
                    field("头像路径", "", false),
                    field("移除已有头像 (yes/no)", "no", false),
                ],
                selected: 0,
            }),
        }
    }

    fn settings_item_count(&self) -> usize {
        let Some(settings) = self.settings.snapshot.as_ref() else {
            return 0;
        };
        match self.settings.section {
            SettingsSection::Models => settings.providers.len() + settings.models.len(),
            SettingsSection::Mcp => settings.mcp_servers.len(),
            SettingsSection::Commands => settings.commands.len(),
            SettingsSection::Subagents => {
                settings.subagent_identities.len() + settings.subagent_roles.len()
            }
        }
    }

    fn selected_setting_is_read_only(&self) -> bool {
        let Some(settings) = self.settings.snapshot.as_ref() else {
            return false;
        };
        match self.settings.section {
            SettingsSection::Models => settings
                .providers
                .get(self.settings.selected)
                .is_some_and(|provider| provider.read_only),
            SettingsSection::Mcp => settings
                .mcp_servers
                .get(self.settings.selected)
                .is_some_and(|server| server.read_only),
            SettingsSection::Commands | SettingsSection::Subagents => false,
        }
    }

    fn edit_selected_setting(&self) -> Option<SettingsEditor> {
        let settings = self.settings.snapshot.as_ref()?;
        let selected = self.settings.selected;
        match self.settings.section {
            SettingsSection::Models => {
                let provider = settings.providers.get(selected)?;
                let default = settings
                    .default_model
                    .as_ref()
                    .filter(|selection| selection.provider_id == provider.id);
                Some(SettingsEditor {
                    title: format!("编辑供应商 {}", provider.name),
                    kind: EditorKind::ModelProvider {
                        original_id: Some(provider.id.clone()),
                    },
                    fields: vec![
                        field("名称", &provider.name, false),
                        field("Base URL", &provider.base_url, false),
                        field("API Key（空值保留）", "", true),
                        field(
                            "启用 (yes/no)",
                            if provider.enabled { "yes" } else { "no" },
                            false,
                        ),
                        field("请求超时秒", &provider.timeout_secs.to_string(), false),
                        field("模型 JSON", &model_specs_json(&provider.models), false),
                        field(
                            "设为默认模型 ID（空=不修改）",
                            default
                                .map(|selection| selection.model_id.as_str())
                                .unwrap_or_default(),
                            false,
                        ),
                        field(
                            "默认推理 (off/high/max)",
                            default
                                .map(|selection| selection.reasoning.as_str())
                                .unwrap_or("off"),
                            false,
                        ),
                    ],
                    selected: 0,
                })
            }
            SettingsSection::Mcp => {
                let server = settings.mcp_servers.get(selected)?;
                Some(SettingsEditor {
                    title: format!("编辑 MCP {}", server.name),
                    kind: EditorKind::McpServer {
                        original_name: Some(server.name.clone()),
                    },
                    fields: vec![
                        field("名称", &server.name, false),
                        field(
                            "传输 (stdio/http)",
                            match server.transport {
                                McpTransport::Stdio => "stdio",
                                McpTransport::Http => "http",
                            },
                            false,
                        ),
                        field("命令", server.command.as_deref().unwrap_or_default(), false),
                        field("参数 JSON", &string_list_json(&server.args), false),
                        field(
                            "工作目录",
                            server
                                .cwd
                                .as_deref()
                                .and_then(std::path::Path::to_str)
                                .unwrap_or_default(),
                            false,
                        ),
                        field("URL", server.url.as_deref().unwrap_or_default(), false),
                        field("Env keys（每行一个）", &server.env_keys.join("\n"), false),
                        field(
                            "Env secret values（逐行对应，空值保留）",
                            &blank_secret_lines(server.env_keys.len()),
                            true,
                        ),
                        field(
                            "Header keys（每行一个）",
                            &server.header_keys.join("\n"),
                            false,
                        ),
                        field(
                            "Header secret values（逐行对应，空值保留）",
                            &blank_secret_lines(server.header_keys.len()),
                            true,
                        ),
                        field(
                            "启用 (yes/no)",
                            if server.enabled { "yes" } else { "no" },
                            false,
                        ),
                        field(
                            "启动超时秒",
                            &server.startup_timeout_secs.to_string(),
                            false,
                        ),
                        field("工具超时秒", &server.tool_timeout_secs.to_string(), false),
                    ],
                    selected: 0,
                })
            }
            SettingsSection::Commands => {
                let command = settings.commands.get(selected)?;
                Some(SettingsEditor {
                    title: format!("编辑命令 /{}", command.name),
                    kind: EditorKind::ManagedCommand {
                        original_name: Some(command.name.clone()),
                    },
                    fields: vec![
                        field("名称", &command.name, false),
                        field("说明", &command.description, false),
                        field("参数提示", &command.argument_hint, false),
                        field("Prompt", &command.prompt, false),
                    ],
                    selected: 0,
                })
            }
            SettingsSection::Subagents => {
                if let Some(identity) = settings.subagent_identities.get(selected) {
                    return Some(SettingsEditor {
                        title: format!("编辑身份 {}", identity.identity.name),
                        kind: EditorKind::SubagentIdentity {
                            original_id: Some(identity.identity.id.clone()),
                            avatar_configured: identity.avatar_configured,
                        },
                        fields: vec![
                            field("名称", &identity.identity.name, false),
                            field("头像路径（空=保留）", "", false),
                            field("移除已有头像 (yes/no)", "no", false),
                        ],
                        selected: 0,
                    });
                }
                let role = settings
                    .subagent_roles
                    .get(selected.saturating_sub(settings.subagent_identities.len()))?;
                let model = role.settings.model_selection.as_ref();
                Some(SettingsEditor {
                    title: format!("编辑 {:?} 角色", role.role),
                    kind: EditorKind::SubagentRole(role.role),
                    fields: vec![
                        field(
                            "模型供应商 ID",
                            model
                                .map(|model| model.provider_id.as_str())
                                .unwrap_or_default(),
                            false,
                        ),
                        field(
                            "模型 ID",
                            model
                                .map(|model| model.model_id.as_str())
                                .unwrap_or_default(),
                            false,
                        ),
                        field(
                            "推理 (off/high/max)",
                            model.map(|model| model.reasoning.as_str()).unwrap_or("off"),
                            false,
                        ),
                        field("Prompt 后缀", &role.settings.prompt_suffix, false),
                        field("超时秒", &role.settings.timeout_secs.to_string(), false),
                        field(
                            "工具轮次",
                            &role.settings.max_tool_rounds.to_string(),
                            false,
                        ),
                    ],
                    selected: 0,
                })
            }
        }
    }

    fn selected_delete_target(&self) -> Option<DeleteTarget> {
        let settings = self.settings.snapshot.as_ref()?;
        match self.settings.section {
            SettingsSection::Models => settings
                .providers
                .get(self.settings.selected)
                .map(|provider| DeleteTarget::ModelProvider(provider.id.clone())),
            SettingsSection::Mcp => settings
                .mcp_servers
                .get(self.settings.selected)
                .map(|server| DeleteTarget::McpServer(server.name.clone())),
            SettingsSection::Commands => settings
                .commands
                .get(self.settings.selected)
                .map(|command| DeleteTarget::ManagedCommand(command.name.clone())),
            SettingsSection::Subagents => settings
                .subagent_identities
                .get(self.settings.selected)
                .map(|identity| DeleteTarget::SubagentIdentity(identity.identity.id.clone())),
        }
    }

    fn activate_selected_setting(&mut self) -> Vec<Effect> {
        if self.selected_setting_is_read_only() {
            self.status = Some("该设置来自 morrow.toml，只读且无法编辑。".to_string());
            return Vec::new();
        }
        let Some(settings) = self.settings.snapshot.as_ref() else {
            return Vec::new();
        };
        if self.settings.section == SettingsSection::Models {
            let model_index = self
                .settings
                .selected
                .saturating_sub(settings.providers.len());
            if self.settings.selected >= settings.providers.len()
                && let Some(model) = settings.models.get(model_index)
            {
                let selection = ModelSelection {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                    reasoning: if model.supports_reasoning {
                        self.reasoning
                    } else {
                        ReasoningLevel::Off
                    },
                };
                return vec![Effect::Backend(BackendCommand::Settings(
                    SettingsCommand::SetDefaultModel(selection),
                ))];
            }
        }
        if let Some(editor) = self.edit_selected_setting() {
            self.overlay = Some(Overlay::SettingsEditor(editor));
        }
        Vec::new()
    }

    fn toggle_selected_setting(&self) -> Vec<Effect> {
        let Some(settings) = self.settings.snapshot.as_ref() else {
            return Vec::new();
        };
        if self.settings.section != SettingsSection::Mcp {
            return Vec::new();
        }
        settings
            .mcp_servers
            .get(self.settings.selected)
            .map(|server| {
                vec![Effect::Backend(BackendCommand::Settings(
                    SettingsCommand::SetMcpEnabled {
                        name: server.name.clone(),
                        enabled: !server.enabled,
                    },
                ))]
            })
            .unwrap_or_default()
    }

    fn test_or_discover_selected(&self) -> Vec<Effect> {
        let Some(settings) = self.settings.snapshot.as_ref() else {
            return Vec::new();
        };
        match self.settings.section {
            SettingsSection::Models => settings
                .providers
                .get(self.settings.selected)
                .map(|provider| {
                    vec![Effect::Backend(BackendCommand::Settings(
                        SettingsCommand::DiscoverModels {
                            provider_id: provider.id.clone(),
                        },
                    ))]
                })
                .unwrap_or_default(),
            SettingsSection::Mcp => settings
                .mcp_servers
                .get(self.settings.selected)
                .map(|server| {
                    vec![Effect::Backend(BackendCommand::Settings(
                        SettingsCommand::TestMcpServer {
                            name: server.name.clone(),
                        },
                    ))]
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn handle_editor_key(&mut self, editor: &mut SettingsEditor, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.submit_editor(editor);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('t')
            && matches!(editor.kind, EditorKind::McpServer { .. })
        {
            return self.test_mcp_editor(editor);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            if let Some(field) = editor.fields.get_mut(editor.selected) {
                field.value.clear();
            }
            self.status = None;
            return Vec::new();
        }
        if (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j'))
            || (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter)
        {
            if let Some(field) = editor.fields.get_mut(editor.selected) {
                field.value.push('\n');
            }
            self.status = None;
            return Vec::new();
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                editor.selected = (editor.selected + 1).min(editor.fields.len().saturating_sub(1));
            }
            KeyCode::BackTab | KeyCode::Up => {
                editor.selected = editor.selected.saturating_sub(1);
            }
            KeyCode::Backspace => {
                if let Some(field) = editor.fields.get_mut(editor.selected) {
                    field.value.pop();
                }
                self.status = None;
            }
            KeyCode::Enter if editor.selected + 1 < editor.fields.len() => {
                editor.selected += 1;
            }
            KeyCode::Enter => return self.submit_editor(editor),
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && !character.is_control() =>
            {
                if let Some(field) = editor.fields.get_mut(editor.selected) {
                    field.value.push(character);
                }
                self.status = None;
            }
            _ => {}
        }
        Vec::new()
    }

    fn submit_editor(&mut self, editor: &SettingsEditor) -> Vec<Effect> {
        match self.settings_command_from_editor(editor) {
            Ok(command) => vec![Effect::Backend(BackendCommand::Settings(command))],
            Err(error) => {
                self.status = Some(format!("设置表单错误: {error}"));
                Vec::new()
            }
        }
    }

    fn test_mcp_editor(&mut self, editor: &SettingsEditor) -> Vec<Effect> {
        match self.settings_command_from_editor(editor) {
            Ok(SettingsCommand::SaveMcpServer(draft)) => {
                vec![Effect::Backend(BackendCommand::Settings(
                    SettingsCommand::TestMcpServerDraft(draft),
                ))]
            }
            Ok(_) => Vec::new(),
            Err(error) => {
                self.status = Some(format!("MCP 草稿错误: {error}"));
                Vec::new()
            }
        }
    }

    fn settings_command_from_editor(
        &self,
        editor: &SettingsEditor,
    ) -> Result<SettingsCommand, String> {
        let value = |index: usize| {
            editor
                .fields
                .get(index)
                .map(|field| field.value.trim())
                .unwrap_or_default()
        };
        let raw_value = |index: usize| {
            editor
                .fields
                .get(index)
                .map(|field| field.value.as_str())
                .unwrap_or_default()
        };
        let command = match &editor.kind {
            EditorKind::ModelProvider { original_id } => {
                let current = original_id.as_deref().and_then(|id| {
                    self.settings.snapshot.as_ref().and_then(|settings| {
                        settings.providers.iter().find(|provider| provider.id == id)
                    })
                });
                let timeout_secs = parse_u64(value(4), "请求超时")?;
                if !(1..=600).contains(&timeout_secs) {
                    return Err("请求超时必须在 1 到 600 秒之间".to_string());
                }
                let default_reasoning = parse_reasoning(value(7))?;
                let default_model = (!value(6).is_empty()).then(|| DefaultModelDraft {
                    model_id: value(6).to_string(),
                    reasoning: default_reasoning,
                });
                SettingsCommand::SaveModelProvider(ModelProviderDraft {
                    id: original_id.clone().unwrap_or_default(),
                    name: value(0).to_string(),
                    base_url: value(1).to_string(),
                    api_key: SecretValue::new(value(2)),
                    enabled: parse_yes_no(value(3), "启用状态")?,
                    read_only: current.is_some_and(|provider| provider.read_only),
                    timeout_secs,
                    models: parse_model_specs(value(5))?,
                    default_model,
                })
            }
            EditorKind::McpServer { original_name } => {
                let current = original_name.as_deref().and_then(|name| {
                    self.settings.snapshot.as_ref().and_then(|settings| {
                        settings
                            .mcp_servers
                            .iter()
                            .find(|server| server.name == name)
                    })
                });
                let transport = match value(1).to_ascii_lowercase().as_str() {
                    "stdio" => McpTransport::Stdio,
                    "http" => McpTransport::Http,
                    _ => return Err("MCP 传输必须是 stdio 或 http".to_string()),
                };
                let startup_timeout_secs = parse_u64(value(11), "启动超时")?;
                let tool_timeout_secs = parse_u64(value(12), "工具超时")?;
                if startup_timeout_secs == 0 || tool_timeout_secs == 0 {
                    return Err("MCP 超时必须大于零".to_string());
                }
                SettingsCommand::SaveMcpServer(McpServerDraft {
                    original_name: original_name.clone(),
                    name: value(0).to_string(),
                    transport,
                    command: value(2).to_string(),
                    args: parse_string_list(value(3), "参数")?,
                    cwd: (!value(4).is_empty()).then(|| PathBuf::from(value(4))),
                    url: (!value(5).is_empty()).then(|| value(5).to_string()),
                    env: secret_map_from_lines(value(6), raw_value(7), "环境变量")?,
                    headers: secret_map_from_lines(value(8), raw_value(9), "HTTP header")?,
                    enabled: parse_yes_no(value(10), "启用状态")?,
                    startup_timeout_secs,
                    tool_timeout_secs,
                    read_only: current.is_some_and(|server| server.read_only),
                    source: current.map_or(McpServerSource::MorrowManaged, |server| server.source),
                })
            }
            EditorKind::McpImport => SettingsCommand::ImportMcpServers {
                source: editor
                    .fields
                    .first()
                    .map(|field| field.value.clone())
                    .unwrap_or_default(),
            },
            EditorKind::ManagedCommand { original_name } => {
                SettingsCommand::SaveManagedCommand(ManagedCommandDraft {
                    original_name: original_name.clone(),
                    name: value(0).to_string(),
                    description: value(1).to_string(),
                    argument_hint: value(2).to_string(),
                    prompt: editor
                        .fields
                        .get(3)
                        .map(|field| field.value.clone())
                        .unwrap_or_default(),
                })
            }
            EditorKind::SubagentIdentity {
                original_id,
                avatar_configured,
            } => {
                let avatar_path = (!value(1).is_empty()).then(|| PathBuf::from(value(1)));
                let remove_avatar = parse_yes_no(value(2), "移除头像")?;
                if avatar_path.is_some() && remove_avatar {
                    return Err("不能同时导入并移除头像".to_string());
                }
                if remove_avatar && !avatar_configured {
                    return Err("当前身份没有可移除的头像".to_string());
                }
                SettingsCommand::SaveSubagentIdentity(SubagentIdentityDraft {
                    original_id: original_id.clone(),
                    identity: SubagentIdentity {
                        id: original_id.clone().unwrap_or_default(),
                        name: value(0).to_string(),
                    },
                    avatar_path,
                    remove_avatar,
                })
            }
            EditorKind::SubagentRole(role) => {
                let reasoning = parse_reasoning(value(2))?;
                if value(0).is_empty() != value(1).is_empty() {
                    return Err("角色模型供应商 ID 和模型 ID 必须同时填写或同时留空".to_string());
                }
                let model_selection = (!value(0).is_empty()).then(|| ModelSelection {
                    provider_id: value(0).to_string(),
                    model_id: value(1).to_string(),
                    reasoning,
                });
                let prompt_suffix = editor
                    .fields
                    .get(3)
                    .map(|field| field.value.clone())
                    .unwrap_or_default();
                if prompt_suffix.chars().count() > MAX_SUBAGENT_PROMPT_SUFFIX_CHARS {
                    return Err(format!(
                        "Prompt 后缀不能超过 {MAX_SUBAGENT_PROMPT_SUFFIX_CHARS} 个字符"
                    ));
                }
                let timeout_secs = parse_u64(value(4), "角色超时")?;
                if !(MIN_SUBAGENT_TIMEOUT_SECS..=MAX_SUBAGENT_TIMEOUT_SECS).contains(&timeout_secs)
                {
                    return Err(format!(
                        "角色超时必须在 {MIN_SUBAGENT_TIMEOUT_SECS} 到 {MAX_SUBAGENT_TIMEOUT_SECS} 秒之间"
                    ));
                }
                let max_tool_rounds = parse_usize(value(5), "工具轮次")?;
                if !(MIN_SUBAGENT_TOOL_ROUNDS..=MAX_SUBAGENT_TOOL_ROUNDS).contains(&max_tool_rounds)
                {
                    return Err(format!(
                        "工具轮次必须在 {MIN_SUBAGENT_TOOL_ROUNDS} 到 {MAX_SUBAGENT_TOOL_ROUNDS} 之间"
                    ));
                }
                SettingsCommand::SaveSubagentRole(SubagentRoleView {
                    role: *role,
                    settings: SubagentRoleOverride {
                        model_selection,
                        prompt_suffix,
                        timeout_secs,
                        max_tool_rounds,
                    },
                })
            }
        };
        Ok(command)
    }

    fn cycle_model(&mut self) -> Vec<Effect> {
        if self.models.is_empty() {
            self.status = Some("没有可用模型，请先打开设置。".to_string());
            return self.open_settings();
        }
        let current = self.current_model();
        let index = current
            .as_ref()
            .and_then(|current| {
                self.models.iter().position(|model| {
                    model.provider_id == current.provider_id && model.model_id == current.model_id
                })
            })
            .map_or(0, |index| (index + 1) % self.models.len());
        let option = self.models[index].clone();
        let selection = ModelSelection {
            provider_id: option.provider_id,
            model_id: option.model_id,
            reasoning: if option.supports_reasoning {
                self.reasoning
            } else {
                ReasoningLevel::Off
            },
        };
        let Some(session_id) = self.active_session_id.clone() else {
            self.status = Some(format!("已选择模型 {}（创建会话后生效）", option.label));
            return Vec::new();
        };
        let reasoning_changed = !option.supports_reasoning && self.reasoning != ReasoningLevel::Off;
        if !option.supports_reasoning {
            self.reasoning = ReasoningLevel::Off;
            self.session_preferences
                .entry(session_id.clone())
                .or_insert(SessionPreference {
                    permissions: self.permissions,
                    reasoning: ReasoningLevel::Off,
                })
                .reasoning = ReasoningLevel::Off;
        }
        if let Some(info) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            info.model = Some(selection.clone());
        }
        self.status = Some(format!("模型：{}", option.label));
        self.schedule_context_estimate(1);
        let mut effects = vec![Effect::Backend(BackendCommand::SetSessionModel {
            session_id,
            selection,
        })];
        if reasoning_changed {
            effects.push(Effect::PersistState);
        }
        effects
    }

    fn cycle_reasoning(&mut self) -> Vec<Effect> {
        if let Some(current) = self.current_model()
            && self.models.iter().any(|model| {
                model.provider_id == current.provider_id
                    && model.model_id == current.model_id
                    && !model.supports_reasoning
            })
        {
            self.reasoning = ReasoningLevel::Off;
            self.status = Some("当前模型不支持推理级别。".to_string());
            self.schedule_context_estimate(1);
            return vec![Effect::PersistState];
        }
        self.reasoning = match self.reasoning {
            ReasoningLevel::Off => ReasoningLevel::High,
            ReasoningLevel::High => ReasoningLevel::Max,
            ReasoningLevel::Max => ReasoningLevel::Off,
        };
        if let Some(session_id) = &self.active_session_id {
            self.session_preferences
                .entry(session_id.clone())
                .or_insert(SessionPreference {
                    permissions: self.permissions,
                    reasoning: self.reasoning,
                })
                .reasoning = self.reasoning;
        }
        let mut effects = vec![Effect::PersistState];
        if let (Some(session_id), Some(mut selection)) =
            (self.active_session_id.clone(), self.current_model())
        {
            selection.reasoning = self.reasoning;
            if let Some(info) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                info.model = Some(selection.clone());
            }
            effects.push(Effect::Backend(BackendCommand::SetSessionModel {
                session_id,
                selection,
            }));
        }
        self.status = Some(format!("推理级别：{}", self.reasoning.as_str()));
        self.schedule_context_estimate(1);
        effects
    }

    fn cycle_permissions(&mut self) -> Vec<Effect> {
        use agent_protocol::{PermissionMode, ShellPolicy};

        self.permissions = match (self.permissions.mode, self.permissions.shell) {
            (PermissionMode::ReadOnly, _) => PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Prompt,
            },
            (PermissionMode::WorkspaceWrite, ShellPolicy::Prompt) => PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Allow,
            },
            (PermissionMode::WorkspaceWrite, ShellPolicy::Allow) => {
                PermissionProfile::for_mode(PermissionMode::DangerFullAccess)
            }
            _ => PermissionProfile::for_mode(PermissionMode::ReadOnly),
        };
        self.stored_permissions = self.permissions;
        if let Some(session_id) = &self.active_session_id {
            self.session_preferences
                .entry(session_id.clone())
                .or_insert(SessionPreference {
                    permissions: self.permissions,
                    reasoning: self.reasoning,
                })
                .permissions = self.permissions;
        }
        if let Some(session_id) = &self.active_session_id
            && let Some(info) = self
                .sessions
                .iter_mut()
                .find(|session| &session.id == session_id)
        {
            info.permissions = self.permissions;
        }
        self.status = Some(format!(
            "权限：{} / shell {}",
            self.permissions.mode.as_str(),
            self.permissions.shell.as_str()
        ));
        self.schedule_context_estimate(1);
        vec![Effect::PersistState]
    }
}
