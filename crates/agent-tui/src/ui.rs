use std::borrow::Cow;
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use agent_protocol::{
    ApprovalAction, ApprovalOrigin, Message as ProtocolMessage, Role, SubagentInstanceStatus,
    ToolExecutionSummary, TurnStatus, TurnStep, TurnStepKind,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Widget, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::backend::{McpTransport, SettingsSnapshot};
use crate::input::sanitize_terminal_text;
use crate::state::{
    AppState, BottomPanel, CompletionKind, InspectorTab, LayoutMode, MainPage, Overlay,
    SettingsSection,
};
use crate::terminal::{InlineRender, ScrollbackFrame};
use crate::theme;

const SPINNER: &[char] = &['⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽', '⣾', '⣶', '⣧'];

/// Render the complete TUI from the reducer state.
pub fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let _ = render_inline(frame, state);
}

pub(crate) fn render_inline(frame: &mut Frame<'_>, state: &mut AppState) -> InlineRender {
    let area = frame.area();
    state.terminal_size = (area.width, area.height);
    frame.render_widget(Clear, area);

    if state.layout_mode() == LayoutMode::TooSmall {
        render_too_small(frame, area, state);
        return InlineRender::default();
    }

    let page = area.inner(Margin {
        horizontal: if area.width >= 54 { 2 } else { 1 },
        vertical: 0,
    });
    let panel_height = bottom_panel_height(state, page);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(panel_height),
            Constraint::Length(2),
        ])
        .split(page);

    let scrollback = if chat_is_empty(state) {
        render_welcome(frame, rows[0], state);
        None
    } else {
        render_transcript(frame, rows[0], state)
    };

    if let Some(approval) = state.approvals.front().cloned() {
        render_approval(frame, rows[1], state, &approval);
    } else if let Some(overlay) = state.overlay.clone() {
        render_overlay(frame, rows[1], state, &overlay);
    } else {
        match state.page {
            MainPage::Chat => {
                let composer_width = usize::from(rows[1].width.saturating_sub(4)).max(1);
                let layout = composer_layout(
                    state.composer.text(),
                    state.composer.cursor(),
                    composer_width,
                );
                render_composer(frame, rows[1], state, layout);
            }
            MainPage::Sessions => render_sessions(frame, rows[1], state),
            MainPage::Inspector => render_inspector(frame, rows[1], state),
            MainPage::Settings => render_settings(frame, rows[1], state),
        }
    }
    render_footer(frame, rows[2], state);
    InlineRender { scrollback }
}

/// Render only durable conversation content before returning control to the shell.
pub(crate) fn render_exit(frame: &mut Frame<'_>, state: &mut AppState) -> InlineRender {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let content = area.inner(Margin {
        horizontal: if area.width >= 54 { 2 } else { 1 },
        vertical: 0,
    });
    if chat_is_empty(state) {
        render_welcome(frame, content, state);
        InlineRender::default()
    } else {
        if let Some(view) = state.active_view_mut() {
            view.at_bottom = true;
        }
        InlineRender {
            scrollback: render_transcript(frame, content, state),
        }
    }
}

fn bottom_panel_height(state: &AppState, area: Rect) -> u16 {
    let maximum = area.height.saturating_sub(5).max(1);
    let requested = match state.bottom_panel() {
        BottomPanel::Approval => 14,
        BottomPanel::SettingsEditor => {
            if let Some(Overlay::SettingsEditor(editor)) = &state.overlay {
                (editor.fields.len() as u16)
                    .saturating_mul(2)
                    .saturating_add(4)
            } else {
                8
            }
        }
        BottomPanel::Help => 18,
        BottomPanel::ActionPalette => 11,
        BottomPanel::ExitConfirm | BottomPanel::ConfirmDelete => 8,
        BottomPanel::SubagentFollowUp => 7,
        BottomPanel::Composer => {
            let width = usize::from(area.width.saturating_sub(4)).max(1);
            let layout = composer_layout(state.composer.text(), state.composer.cursor(), width);
            (layout.lines.len() as u16).saturating_add(2).clamp(3, 8)
        }
        BottomPanel::Sessions => (state.sessions.len() as u16)
            .saturating_mul(2)
            .saturating_add(2)
            .clamp(7, 16),
        BottomPanel::Inspector => 16,
        BottomPanel::Settings => 18,
    };
    requested.min(maximum)
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let message = Text::from(vec![
        Line::styled("Morrow · 终端窗口太小", emphasis(state)),
        Line::from(format!(
            "当前 {}×{}，至少需要 48×12",
            area.width, area.height
        )),
        Line::from("请放大窗口，或按 Ctrl+Q 退出。"),
    ]);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let hint = if !state.approvals.is_empty() {
        "Y 批准 · N 拒绝 · ↑↓/PageUp/PageDown 滚动"
    } else if let Some(overlay) = &state.overlay {
        match overlay {
            Overlay::Help => "F1 / Esc 返回",
            Overlay::ActionPalette { .. } => "↑↓ 选择 · Enter 执行 · Esc 返回",
            Overlay::ExitConfirm => "按面板提示选择退出方式 · Esc 返回",
            Overlay::ConfirmDelete(_) => "Y 确认 · N / Esc 返回",
            Overlay::SettingsEditor(_) => "Ctrl+S 保存 · Esc 返回设置",
            Overlay::SubagentFollowUp { .. } => "Enter 发送 · Esc 返回 Inspector",
        }
    } else {
        match state.page {
            MainPage::Chat => "Enter 发送 · Ctrl+J 换行 · Ctrl+B 会话 · Ctrl+G 检查器 · F1 帮助",
            MainPage::Sessions => {
                "↑↓ 选择 · Enter 打开 · n 新建 · a 归档 · u 恢复 · / 搜索 · Esc 返回"
            }
            MainPage::Inspector => "Tab 切换 · ↑↓ 选择 · Y 复制 · f follow-up · x 取消 · Esc 返回",
            MainPage::Settings => "←→ 分区 · ↑↓ 选择 · n 新建 · e 编辑 · d 删除 · Esc 返回",
        }
    };
    let workspace = sanitize_terminal_text(&state.workspace.display().to_string());
    let session = state
        .active_info()
        .map(|info| info.title.as_str())
        .unwrap_or("未选择会话");
    let activity = if state.has_active_work() {
        format!("{} Morrow 正在工作", SPINNER[state.spinner % SPINNER.len()])
    } else {
        hint.to_string()
    };
    let status = state.status.as_deref().unwrap_or(&activity);

    let model = state
        .active_info()
        .and_then(|info| info.model.as_ref())
        .map_or("未配置模型", |model| model.model_id.as_str());
    let context = match (&state.context.estimate, state.context.loading) {
        (Some(estimate), _) => {
            let percent = estimate
                .used_tokens
                .saturating_mul(100)
                .checked_div(estimate.input_budget_tokens)
                .unwrap_or_default();
            format!(
                "context {percent}% {}/{}",
                compact_number(estimate.used_tokens),
                compact_number(estimate.input_budget_tokens)
            )
        }
        (None, true) => "context …".to_string(),
        _ => "context --".to_string(),
    };
    let permission = format!(
        "{} / shell {}",
        state.permissions.mode.as_str(),
        state.permissions.shell.as_str()
    );
    let right = format!("{model} · {} · {context}", state.reasoning.as_str());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(left_right_line(
            &format!("{workspace} · {session}"),
            status,
            rows[0].width,
            state,
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(left_right_line(&permission, &right, rows[1].width, state)),
        rows[1],
    );
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let query = state.session_search.to_lowercase();
    let filtered = state
        .sessions
        .iter()
        .filter(|session| {
            query.is_empty()
                || session.id.to_lowercase().contains(&query)
                || session.title.to_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    let items = if filtered.is_empty() {
        vec![ListItem::new(Line::styled(
            if state.sessions.is_empty() {
                "还没有会话，按 n 创建"
            } else {
                "没有匹配的会话"
            },
            muted(state),
        ))]
    } else {
        filtered
            .iter()
            .enumerate()
            .map(|(position, session)| {
                let selected = position == state.selected_session;
                let active = state.active_session_id.as_deref() == Some(session.id.as_str());
                let unread = state.views.get(&session.id).map_or(0, |view| view.unread);
                let marker = if active { "●" } else { " " };
                let activity = if session.running {
                    " 运行中"
                } else if session.archived {
                    " 已归档"
                } else {
                    ""
                };
                let unread = if unread > 0 {
                    format!(" +{unread}")
                } else {
                    String::new()
                };
                let mut line = format!("{marker} {}{activity}{unread}", session.title);
                if session.id != session.title {
                    line.push_str(&format!("\n  {}", session.id));
                }
                ListItem::new(line).style(if selected {
                    selected_style(state)
                } else if active {
                    accent(state)
                } else {
                    Style::default()
                })
            })
            .collect()
    };
    let search = if state.session_search_active {
        format!(" 搜索: {}_ ", state.session_search)
    } else if state.session_search.is_empty() {
        " 会话 ".to_string()
    } else {
        format!(" 会话 · /{} ", state.session_search)
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .title(search)
        .title_bottom(" Esc 返回 · n 新建 · Enter 打开 · a/u 归档/恢复 ");
    let mut list_state = ListState::default().with_selected(
        (!filtered.is_empty())
            .then_some(state.selected_session.min(filtered.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(List::new(items).block(block), area, &mut list_state);
}

struct ChatRender {
    text: Text<'static>,
    durable_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposerLayout {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

fn chat_is_empty(state: &AppState) -> bool {
    let Some(view) = state.active_view() else {
        return state.active_session_id.is_none();
    };
    let persisted_empty = view.snapshot.as_ref().is_none_or(|snapshot| {
        snapshot.session.turns.is_empty() && snapshot.session.context.summary.is_none()
    });
    persisted_empty
        && view.live.user_prompt.is_none()
        && view.live.reasoning.is_empty()
        && view.live.text.is_empty()
        && view.live.warnings.is_empty()
        && view.live.tools.is_empty()
        && view.live.error.is_none()
        && !view.live.awaiting_save
}

fn render_welcome(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let workspace = state.workspace.display().to_string();
    let session = state
        .active_info()
        .map(|info| info.title.as_str())
        .unwrap_or("尚未选择");
    let model = state
        .active_info()
        .and_then(|info| info.model.as_ref())
        .map_or("未配置", |model| model.model_id.as_str());
    let enabled_mcp = state.settings.snapshot.as_ref().map_or(0, |settings| {
        settings
            .mcp_servers
            .iter()
            .filter(|server| server.enabled)
            .count()
    });
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("◆ ", accent(state).add_modifier(Modifier::BOLD)),
            Span::styled("Morrow", emphasis(state)),
        ]),
        Line::styled("  输入消息开始工作，按 F1 查看帮助。", muted(state)),
        Line::from(""),
        welcome_value("工作区", &workspace, state),
        welcome_value("会话", session, state),
        welcome_value("模型", model, state),
        welcome_value(
            "状态",
            &format!("v{} · MCP {enabled_mcp}", env!("CARGO_PKG_VERSION")),
            state,
        ),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn welcome_value(label: &str, value: &str, state: &AppState) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<6}"),
            muted(state).add_modifier(Modifier::BOLD),
        ),
        Span::raw(sanitize_terminal_text(value)),
    ])
}

fn render_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
) -> Option<ScrollbackFrame> {
    let transcript_area = area;
    let width = transcript_area.width.max(1);
    let rendered = build_chat_text(state, width);
    let content_height = wrapped_text_height(&rendered.text, width);
    let paragraph = Paragraph::new(rendered.text.clone()).wrap(Wrap { trim: false });
    let viewport_height = transcript_area.height;
    let max_scroll = content_height.saturating_sub(viewport_height);
    let mut scroll = 0;
    if let Some(view) = state.active_view_mut() {
        if view.at_bottom {
            view.scroll = max_scroll;
            view.unread = 0;
        } else {
            view.scroll = view.scroll.min(max_scroll);
            if view.scroll >= max_scroll {
                view.at_bottom = true;
                view.unread = 0;
            }
        }
        scroll = view.scroll;
    }
    frame.render_widget(paragraph.scroll((scroll, 0)), transcript_area);

    let durable_text = Text {
        alignment: rendered.text.alignment,
        style: rendered.text.style,
        lines: rendered
            .text
            .lines
            .iter()
            .take(rendered.durable_lines)
            .cloned()
            .collect(),
    };
    let durable_height = wrapped_text_height(&durable_text, width);
    let commit_height = max_scroll.min(durable_height);
    if commit_height == 0 {
        return state
            .active_session_id
            .clone()
            .map(|session_id| ScrollbackFrame {
                session_id: Some(session_id),
                x: transcript_area.x,
                rows: Buffer::empty(Rect::new(0, 0, width, 0)),
            });
    }

    let mut rows = Buffer::empty(Rect::new(0, 0, width, commit_height));
    Paragraph::new(rendered.text)
        .wrap(Wrap { trim: false })
        .render(rows.area, &mut rows);
    Some(ScrollbackFrame {
        session_id: state.active_session_id.clone(),
        x: transcript_area.x,
        rows,
    })
}

fn left_right_line(left: &str, right: &str, width: u16, state: &AppState) -> Line<'static> {
    let width = usize::from(width);
    let right = truncate_to_width(right, width / 2);
    let right_width = UnicodeWidthStr::width(right.as_str());
    let left = truncate_to_width(left, width.saturating_sub(right_width + 1));
    let gap = width.saturating_sub(UnicodeWidthStr::width(left.as_str()) + right_width);
    Line::from(vec![
        Span::styled(left, muted(state)),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, muted(state)),
    ])
}

fn build_chat_text(state: &mut AppState, width: u16) -> ChatRender {
    let Some(session_id) = state.active_session_id.clone() else {
        return ChatRender {
            text: Text::from(vec![
                Line::styled("欢迎使用 Morrow", emphasis(state)),
                Line::from(""),
                Line::from("按 Ctrl+B 打开会话页，然后按 n 创建第一个会话。"),
            ]),
            durable_lines: 3,
        };
    };
    let Some(view) = state.views.get(&session_id) else {
        return ChatRender {
            text: Text::from(Line::styled("正在加载会话…", muted(state))),
            durable_lines: 1,
        };
    };
    let snapshot = view.snapshot.clone();
    let live = view.live.clone();
    let selected_message = view.selected_message;
    let reasoning_expanded = state.reasoning_expanded;
    let no_color = state.no_color;
    let mut output = Text::default();

    if let Some(snapshot) = snapshot {
        if let Some(summary) = snapshot.session.context.summary.as_deref() {
            output.lines.push(Line::styled(
                "◈ 已压缩上下文",
                theme::thinking(state.no_color),
            ));
            push_plain(&mut output, summary, theme::thinking(state.no_color));
            output.lines.push(Line::from(""));
        }
        for (index, record) in snapshot.session.turns.iter().enumerate() {
            let selected = index == selected_message;
            if let Some(content) = &record.turn.user_message.content {
                push_user_message(&mut output, content, selected, width, state);
            } else {
                push_user_message(&mut output, "", selected, width, state);
            }

            append_persisted_messages(
                &mut output,
                state,
                &session_id,
                index,
                width,
                &record.messages,
                &record.turn.steps,
                record.turn.assistant_message.as_ref(),
                reasoning_expanded,
                no_color,
            );
            if record.turn.status == TurnStatus::Failed {
                append_error_card(
                    &mut output,
                    "本轮执行失败",
                    record.turn.error.as_deref().unwrap_or("本轮执行失败"),
                    width,
                    state,
                );
            }
            output.lines.push(Line::from(""));
        }
    }

    let durable_lines = output.lines.len();

    if let Some(prompt) = live.user_prompt {
        push_user_message(&mut output, &prompt, true, width, state);
    }
    if !live.reasoning.is_empty() {
        if reasoning_expanded {
            output
                .lines
                .push(Line::styled("thinking…", theme::thinking(state.no_color)));
            push_plain(
                &mut output,
                &live.reasoning,
                theme::thinking(state.no_color),
            );
        } else {
            output.lines.push(Line::styled(
                "▸ thinking… · Ctrl+O 展开",
                theme::thinking(state.no_color),
            ));
        }
    }
    if !live.text.is_empty() {
        let rendered = cached_markdown(state, &session_id, usize::MAX, width, &live.text, no_color);
        output.lines.extend(rendered.lines);
    }
    for warning in live.warnings {
        output
            .lines
            .push(Line::styled(format!("! {warning}"), warning_style(state)));
    }
    for tool in live.tools {
        append_tool_card(
            &mut output,
            &tool.name,
            tool.status,
            None,
            tool.result.as_deref(),
            tool.summary.as_ref(),
            tool.summary
                .as_ref()
                .and_then(|summary| summary.error.as_deref()),
            width,
            state,
        );
    }
    if let Some(error) = live.error {
        append_error_card(&mut output, "执行错误", &error, width, state);
    }
    if live.awaiting_save {
        output
            .lines
            .push(Line::styled("  正在写入会话记录…", muted(state)));
    }
    ChatRender {
        text: output,
        durable_lines,
    }
}

fn push_user_message(
    output: &mut Text<'static>,
    value: &str,
    selected: bool,
    width: u16,
    state: &AppState,
) {
    let sanitized = sanitize_terminal_text(value);
    if output.lines.last().is_some_and(|line| line.width() > 0) {
        output.lines.push(Line::from(""));
    }
    let style = if selected {
        selected_style(state)
    } else {
        theme::user_card(state.no_color)
    };
    let lines = if sanitized.is_empty() {
        vec![String::new()]
    } else {
        sanitized.lines().map(str::to_string).collect::<Vec<_>>()
    };
    for (index, line) in lines.into_iter().enumerate() {
        let marker = if selected && index == 0 { "▌ " } else { "  " };
        output.lines.push(Line::styled(
            pad_to_width(&format!("{marker}{line}"), usize::from(width)),
            style,
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn append_persisted_messages(
    output: &mut Text<'static>,
    state: &mut AppState,
    session_id: &str,
    turn_index: usize,
    width: u16,
    messages: &[ProtocolMessage],
    steps: &[TurnStep],
    fallback: Option<&ProtocolMessage>,
    reasoning_expanded: bool,
    no_color: bool,
) {
    let results = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| {
            message
                .tool_call_id
                .as_deref()
                .map(|tool_call_id| (tool_call_id, message))
        })
        .collect::<HashMap<_, _>>();
    let result_counts = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .fold(HashMap::new(), |mut counts, tool_call_id| {
            *counts.entry(tool_call_id).or_insert(0usize) += 1;
            counts
        });
    let tool_steps = steps
        .iter()
        .filter(|step| step.kind == TurnStepKind::ToolCall)
        .filter_map(|step| {
            step.tool_call_id
                .as_deref()
                .map(|tool_call_id| (tool_call_id, step))
        })
        .collect::<HashMap<_, _>>();
    let step_counts = steps
        .iter()
        .filter(|step| step.kind == TurnStepKind::ToolCall)
        .filter_map(|step| step.tool_call_id.as_deref())
        .fold(HashMap::new(), |mut counts, tool_call_id| {
            *counts.entry(tool_call_id).or_insert(0usize) += 1;
            counts
        });
    let call_counts = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .fold(HashMap::new(), |mut counts, call| {
            *counts.entry(call.id.as_str()).or_insert(0usize) += 1;
            counts
        });
    let mut matched_results = std::collections::HashSet::new();
    let mut matched_calls = std::collections::HashSet::new();
    let mut rendered_assistant = false;
    for (message_index, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        rendered_assistant = true;
        append_assistant_content(
            output,
            state,
            session_id,
            turn_index
                .saturating_mul(1024)
                .saturating_add(message_index),
            width,
            message,
            reasoning_expanded,
            no_color,
        );
        if let Some(tool_calls) = &message.tool_calls {
            for call in tool_calls {
                matched_calls.insert(call.id.as_str());
                let malformed = call.id.trim().is_empty()
                    || call_counts
                        .get(call.id.as_str())
                        .copied()
                        .unwrap_or_default()
                        > 1
                    || result_counts
                        .get(call.id.as_str())
                        .copied()
                        .unwrap_or_default()
                        > 1
                    || step_counts
                        .get(call.id.as_str())
                        .copied()
                        .unwrap_or_default()
                        > 1;
                if malformed {
                    append_tool_card(
                        output,
                        &call.function.name,
                        TurnStatus::Failed,
                        Some(&call.function.arguments),
                        None,
                        None,
                        Some("工具历史包含缺失或重复的 tool_call_id，无法安全匹配结果"),
                        width,
                        state,
                    );
                    continue;
                }
                let step = tool_steps.get(call.id.as_str()).copied();
                let result = results.get(call.id.as_str()).copied();
                if result.is_some() {
                    matched_results.insert(call.id.as_str());
                }
                let status = step.map_or_else(
                    || {
                        if result.is_some() {
                            TurnStatus::Completed
                        } else {
                            TurnStatus::Running
                        }
                    },
                    |step| step.status,
                );
                append_tool_card(
                    output,
                    &call.function.name,
                    status,
                    Some(&call.function.arguments),
                    result.and_then(|message| message.content.as_deref()),
                    None,
                    step.and_then(|step| step.error.as_deref()),
                    width,
                    state,
                );
            }
        }
    }
    if !rendered_assistant && let Some(message) = fallback {
        append_assistant_content(
            output,
            state,
            session_id,
            turn_index,
            width,
            message,
            reasoning_expanded,
            no_color,
        );
    }
    for message in messages.iter().filter(|message| message.role == Role::Tool) {
        let tool_call_id = message.tool_call_id.as_deref();
        if tool_call_id.is_some_and(|tool_call_id| matched_results.contains(tool_call_id)) {
            continue;
        }
        let step = tool_call_id.and_then(|tool_call_id| {
            (step_counts.get(tool_call_id).copied().unwrap_or_default() == 1)
                .then(|| tool_steps.get(tool_call_id).copied())
                .flatten()
        });
        let duplicate = tool_call_id.is_some_and(|tool_call_id| {
            result_counts.get(tool_call_id).copied().unwrap_or_default() > 1
        });
        append_tool_card(
            output,
            if duplicate {
                "重复工具结果"
            } else {
                step.and_then(|step| step.tool_name.as_deref())
                    .unwrap_or("未知工具结果")
            },
            if duplicate || tool_call_id.is_none() {
                TurnStatus::Failed
            } else {
                step.map_or(TurnStatus::Completed, |step| step.status)
            },
            None,
            message.content.as_deref(),
            None,
            if tool_call_id.is_none() {
                Some("工具结果缺少 tool_call_id")
            } else if duplicate {
                Some("同一 tool_call_id 存在多个工具结果")
            } else {
                step.and_then(|step| step.error.as_deref())
            },
            width,
            state,
        );
    }
    for step in steps.iter().filter(|step| {
        step.kind == TurnStepKind::ToolCall
            && step
                .tool_call_id
                .as_deref()
                .is_none_or(|id| !matched_calls.contains(id))
    }) {
        if step
            .tool_call_id
            .as_deref()
            .is_some_and(|id| results_contained(messages, id))
        {
            continue;
        }
        append_tool_card(
            output,
            step.tool_name.as_deref().unwrap_or("未知工具"),
            step.status,
            None,
            None,
            None,
            step.error.as_deref(),
            width,
            state,
        );
    }
    for step in steps
        .iter()
        .filter(|step| step.kind == TurnStepKind::ModelCall && step.status == TurnStatus::Failed)
    {
        append_error_card(
            output,
            "模型调用失败",
            step.error.as_deref().unwrap_or("模型调用失败"),
            width,
            state,
        );
    }
}

fn results_contained(messages: &[ProtocolMessage], tool_call_id: &str) -> bool {
    messages.iter().any(|message| {
        message.role == Role::Tool && message.tool_call_id.as_deref() == Some(tool_call_id)
    })
}

#[allow(clippy::too_many_arguments)]
fn append_assistant_content(
    output: &mut Text<'static>,
    state: &mut AppState,
    session_id: &str,
    cache_index: usize,
    width: u16,
    message: &ProtocolMessage,
    reasoning_expanded: bool,
    no_color: bool,
) {
    if let Some(reasoning) = message
        .reasoning_content
        .as_deref()
        .filter(|reasoning| !reasoning.is_empty())
    {
        if reasoning_expanded {
            output
                .lines
                .push(Line::styled("thinking", theme::thinking(state.no_color)));
            push_plain(output, reasoning, theme::thinking(state.no_color));
        } else {
            output.lines.push(Line::styled(
                "▸ thinking 已折叠 · Ctrl+O 展开",
                theme::thinking(state.no_color),
            ));
        }
    }
    if let Some(content) = message
        .content
        .as_deref()
        .filter(|content| !content.is_empty())
    {
        let rendered = cached_markdown(state, session_id, cache_index, width, content, no_color);
        output.lines.extend(rendered.lines);
    }
}

#[allow(clippy::too_many_arguments)]
fn append_tool_card(
    output: &mut Text<'static>,
    name: &str,
    status: TurnStatus,
    arguments: Option<&str>,
    result: Option<&str>,
    summary: Option<&ToolExecutionSummary>,
    error: Option<&str>,
    width: u16,
    state: &AppState,
) {
    if output.lines.last().is_some_and(|line| line.width() > 0) {
        output.lines.push(Line::from(""));
    }
    let card = match status {
        TurnStatus::Running => theme::tool_pending(state.no_color),
        TurnStatus::Completed => theme::tool_success(state.no_color),
        TurnStatus::Failed => theme::tool_error(state.no_color),
    };
    output.lines.push(Line::styled(
        pad_to_width(
            &format!(
                "  {} {}",
                turn_status_marker(status),
                sanitize_terminal_text(name)
            ),
            usize::from(width),
        ),
        status_style(state, status)
            .patch(card)
            .add_modifier(Modifier::BOLD),
    ));

    let mut details = Vec::new();
    if let Some(arguments) = arguments.filter(|arguments| !arguments.trim().is_empty()) {
        let pretty = pretty_json_or_text(arguments);
        details.extend(pretty.lines().take(4).map(|line| format!("  {line}")));
    }
    if let Some(summary) = summary {
        for file in &summary.files {
            details.push(format!(
                "  {} {} · {} replacement{}",
                file.operation.as_str(),
                file.path,
                file.replacements,
                if file.replacements == 1 { "" } else { "s" }
            ));
        }
        if let Some(shell) = &summary.shell {
            details.push(format!("  $ {}", shell.command));
            let mut shell_status = shell
                .exit_code
                .map_or_else(|| "exit —".to_string(), |code| format!("exit {code}"));
            if shell.timed_out {
                shell_status.push_str(" · timeout");
            }
            if shell.stdout_truncated || shell.stderr_truncated {
                shell_status.push_str(" · output truncated");
            }
            details.push(format!("  {shell_status}"));
        }
        if let Some(subagent) = &summary.subagent {
            let identity = subagent
                .agent_name
                .as_deref()
                .or(subagent.agent_id.as_deref())
                .unwrap_or("Subagent");
            details.push(format!("  {identity} · {}", subagent.task));
            details.push(format!(
                "  {} model calls · {} tool calls{}",
                subagent.model_calls,
                subagent.tool_calls,
                if subagent.truncated {
                    " · truncated"
                } else {
                    ""
                }
            ));
            if let Some(value) = &subagent.result {
                details.extend(value.lines().take(5).map(|line| format!("  {line}")));
            }
            if let Some(value) = &subagent.error {
                details.extend(value.lines().take(5).map(|line| format!("  {line}")));
            }
        }
        if let Some(diff) = &summary.diff {
            details.extend(diff.lines().take(10).map(|line| format!("  {line}")));
        }
    }
    if let Some(result) = result.filter(|result| !result.trim().is_empty()) {
        let pretty = pretty_json_or_text(result);
        details.extend(pretty.lines().take(8).map(|line| format!("  {line}")));
    }
    if let Some(error) = error.filter(|error| !error.trim().is_empty()) {
        details.extend(
            sanitize_terminal_text(error)
                .lines()
                .take(5)
                .map(|line| format!("  {line}")),
        );
    }
    let truncated = details.len() > 12;
    details.truncate(12);
    let detail_style = muted(state).patch(card);
    for detail in details {
        let trimmed = detail.trim_start();
        let style =
            if trimmed.starts_with('+') || trimmed.starts_with('-') || trimmed.starts_with("@@") {
                diff_style(state, trimmed).patch(card)
            } else {
                detail_style
            };
        output.lines.push(Line::styled(
            pad_to_width(&detail, usize::from(width)),
            style,
        ));
    }
    if truncated {
        output.lines.push(Line::styled(
            pad_to_width("  …更多内容请在 Inspector 中查看", usize::from(width)),
            detail_style,
        ));
    }
}

fn append_error_card(
    output: &mut Text<'static>,
    title: &str,
    error: &str,
    width: u16,
    state: &AppState,
) {
    let card = theme::tool_error(state.no_color);
    output.lines.push(Line::styled(
        pad_to_width(&format!("  × {title}"), usize::from(width)),
        error_style(state).patch(card).add_modifier(Modifier::BOLD),
    ));
    for line in sanitize_terminal_text(error).lines().take(8) {
        output.lines.push(Line::styled(
            pad_to_width(&format!("  {line}"), usize::from(width)),
            error_style(state).patch(card),
        ));
    }
}

fn pretty_json_or_text(value: &str) -> String {
    let sanitized = sanitize_terminal_text(value);
    serde_json::from_str::<serde_json::Value>(&sanitized)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or(sanitized)
}

fn pad_to_width(value: &str, width: usize) -> String {
    let value = truncate_to_width(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{value}{}", " ".repeat(padding))
}

fn cached_markdown(
    state: &mut AppState,
    session_id: &str,
    index: usize,
    width: u16,
    content: &str,
    no_color: bool,
) -> Text<'static> {
    let sanitized = sanitize_terminal_text(content);
    let mut hasher = DefaultHasher::new();
    sanitized.hash(&mut hasher);
    no_color.hash(&mut hasher);
    let key = (session_id.to_string(), index, width, hasher.finish());
    if let Some(rendered) = state.render_cache.get(&key) {
        return rendered.clone();
    }
    let mut rendered = own_text(tui_markdown::from_str(&sanitized));
    if no_color {
        strip_text_colors(&mut rendered);
    } else {
        apply_markdown_theme(&mut rendered);
    }
    if state.render_cache.len() >= 512 {
        state.render_cache.clear();
    }
    state.render_cache.insert(key, rendered.clone());
    rendered
}

fn apply_markdown_theme(text: &mut Text<'_>) {
    map_markdown_style(&mut text.style);
    for line in &mut text.lines {
        map_markdown_style(&mut line.style);
        for span in &mut line.spans {
            map_markdown_style(&mut span.style);
        }
    }
}

fn map_markdown_style(style: &mut Style) {
    style.bg = None;
    style.underline_color = style.underline_color.map(markdown_color);
    style.fg = style.fg.map(markdown_color);
}

fn markdown_color(color: Color) -> Color {
    match color {
        Color::Red | Color::LightRed => theme::ERROR,
        Color::Green | Color::LightGreen => theme::SUCCESS,
        Color::Yellow | Color::LightYellow => theme::WARNING,
        Color::Blue | Color::LightBlue => theme::INFO,
        Color::Cyan | Color::LightCyan | Color::Magenta | Color::LightMagenta => theme::ACCENT,
        Color::Gray | Color::DarkGray => theme::MUTED,
        Color::Reset => Color::Reset,
        _ => theme::TEXT,
    }
}

fn own_text(text: Text<'_>) -> Text<'static> {
    Text {
        alignment: text.alignment,
        style: text.style,
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                alignment: line.alignment,
                style: line.style,
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span {
                        style: span.style,
                        content: Cow::Owned(span.content.into_owned()),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn strip_text_colors(text: &mut Text<'_>) {
    text.style.fg = None;
    text.style.bg = None;
    text.style.underline_color = None;
    for line in &mut text.lines {
        line.style.fg = None;
        line.style.bg = None;
        line.style.underline_color = None;
        for span in &mut line.spans {
            span.style.fg = None;
            span.style.bg = None;
            span.style.underline_color = None;
        }
    }
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &AppState, layout: ComposerLayout) {
    let placeholder = if state.models.is_empty() {
        "配置模型后开始对话…"
    } else if state.active_session_id.is_none() {
        "请先创建或选择会话"
    } else {
        "给 Morrow 发消息…"
    };
    let text = if state.composer.text().is_empty() {
        Text::from(Line::styled(placeholder, muted(state)))
    } else {
        Text::from(
            layout
                .lines
                .iter()
                .cloned()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(accent(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::styled(
            " › ",
            accent(state).add_modifier(Modifier::BOLD),
        )),
        columns[0],
    );
    let viewport_height = usize::from(columns[1].height).max(1);
    let scroll = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(viewport_height);
    frame.render_widget(
        Paragraph::new(text).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        columns[1],
    );

    if let Some(completion) = &state.completion {
        render_completion(frame, area, state, completion);
    }

    if state.overlay.is_none() && state.approvals.is_empty() && columns[1].width > 0 {
        let visible_row = layout.cursor_row.saturating_sub(scroll);
        let x = columns[1]
            .x
            .saturating_add(layout.cursor_column.min(u16::MAX as usize) as u16);
        let y = columns[1]
            .y
            .saturating_add(visible_row.min(u16::MAX as usize) as u16);
        if x < columns[1].right() && y < columns[1].bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

fn render_completion(
    frame: &mut Frame<'_>,
    composer_area: Rect,
    state: &AppState,
    completion: &crate::state::CompletionPopup,
) {
    let count = completion.items.len().min(8) as u16;
    let height = count.saturating_add(2).max(3);
    let width = composer_area.width.saturating_sub(2).clamp(24, 72);
    let x = composer_area.x.saturating_add(1);
    let y = composer_area.y.saturating_sub(height);
    let area = Rect::new(
        x,
        y,
        width.min(composer_area.right().saturating_sub(x)),
        height,
    );
    let kind = match completion.kind {
        CompletionKind::ManagedCommand => " 命令补全 ",
        CompletionKind::WorkspacePath => " 路径补全 ",
    };
    let items = completion
        .items
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, item)| {
            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", item.label),
                    if index == completion.selected {
                        selected_style(state)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(item.detail.clone(), muted(state)),
            ]);
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(accent(state))
                .title(kind),
        ),
        area,
    );
}

fn composer_layout(text: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    let mut lines = Vec::new();
    let mut cursor_position = None;
    let mut line_start = 0;

    for segment in text.split('\n') {
        let mut visual_line = String::new();
        let mut visual_width = 0;
        let mut visual_start = line_start;
        let span = Span::raw(segment);

        if cursor == line_start {
            cursor_position = Some((lines.len(), 0));
        }

        for grapheme in span.styled_graphemes(Style::default()) {
            let local_start = grapheme.symbol.as_ptr() as usize - segment.as_ptr() as usize;
            let grapheme_start = line_start + local_start;
            let grapheme_end = grapheme_start + grapheme.symbol.len();
            let grapheme_width = UnicodeWidthStr::width(grapheme.symbol);

            if !visual_line.is_empty() && visual_width + grapheme_width > width {
                lines.push(std::mem::take(&mut visual_line));
                visual_width = 0;
                visual_start = grapheme_start;
            }

            if (visual_start..=grapheme_end).contains(&cursor) {
                let prefix_end = cursor
                    .saturating_sub(visual_start)
                    .min(grapheme_end.saturating_sub(visual_start));
                let prefix = &text[visual_start..visual_start + prefix_end];
                cursor_position = Some((lines.len(), UnicodeWidthStr::width(prefix)));
            }

            visual_line.push_str(grapheme.symbol);
            visual_width += grapheme_width;
        }

        lines.push(visual_line);
        let segment_end = line_start + segment.len();
        if cursor == segment_end {
            cursor_position = Some((lines.len() - 1, visual_width));
        }
        line_start = segment_end.saturating_add(1);
    }

    let (mut cursor_row, mut cursor_column) = cursor_position.unwrap_or((0, 0));
    if cursor_column >= width {
        cursor_row += cursor_column / width;
        cursor_column %= width;
        while lines.len() <= cursor_row {
            lines.push(String::new());
        }
    }

    ComposerLayout {
        lines,
        cursor_row,
        cursor_column,
    }
}

fn render_inspector(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let titles = ["Run", "Subagent"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(match state.inspector_tab {
                InspectorTab::Run => 0,
                InspectorTab::Subagents => 1,
            })
            .highlight_style(selected_style(state))
            .divider(" │ ")
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(" 检查器 "),
            ),
        rows[0],
    );
    match state.inspector_tab {
        InspectorTab::Run => render_run_inspector(frame, rows[1], state),
        InspectorTab::Subagents => render_subagents(frame, rows[1], state),
    }
}

fn render_run_inspector(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let mut lines = Vec::new();
    let mut selected_line = None;
    let Some(view) = state.active_view() else {
        frame.render_widget(
            Paragraph::new("选择会话后查看运行状态")
                .style(muted(state))
                .block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
            area,
        );
        return;
    };
    if let Some(snapshot) = &view.snapshot {
        for (turn_index, record) in snapshot.session.turns.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "Turn {}  {} {}",
                    turn_index + 1,
                    turn_status_marker(record.turn.status),
                    status_label(record.turn.status)
                ),
                status_style(state, record.turn.status),
            ));
            if let Some(model) = &record.turn.model {
                lines.push(Line::styled(
                    format!("  模型 {}/{}", model.provider_name, model.model_name),
                    muted(state),
                ));
            }
            for step in &record.turn.steps {
                let name = match step.kind {
                    TurnStepKind::ModelCall => "模型步骤".to_string(),
                    TurnStepKind::ToolCall => {
                        format!("工具 {}", step.tool_name.as_deref().unwrap_or("未知"))
                    }
                };
                lines.push(Line::styled(
                    format!("  {} {name}", turn_status_marker(step.status)),
                    status_style(state, step.status),
                ));
                if let Some(error) = &step.error {
                    lines.push(Line::styled(format!("    {error}"), error_style(state)));
                }
            }
            append_persisted_tool_details(&mut lines, &record.messages, state);
            if let Some(error) = &record.turn.error {
                lines.push(Line::styled(format!("  错误: {error}"), error_style(state)));
            }
        }
    }
    if !view.live.tools.is_empty() || view.live.user_prompt.is_some() {
        lines.push(Line::styled("当前运行", emphasis(state)));
        for (index, tool) in view.live.tools.iter().enumerate() {
            if index == state.selected_inspector {
                selected_line = Some(lines.len());
            }
            lines.push(Line::styled(
                format!(
                    "{} {} {}",
                    selection_marker(index, state.selected_inspector),
                    turn_status_marker(tool.status),
                    tool.name
                ),
                if index == state.selected_inspector {
                    selected_style(state)
                } else {
                    status_style(state, tool.status)
                },
            ));
            let detail = tool.plain_text();
            if detail.lines().count() > 1 {
                for line in detail.lines().skip(1).take(12) {
                    lines.push(Line::styled(format!("    {line}"), muted(state)));
                }
            }
        }
        if let Some(error) = &view.live.error {
            lines.push(Line::styled(format!("错误: {error}"), error_style(state)));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("还没有运行记录", muted(state)));
    }
    let visible_height = usize::from(area.height.saturating_sub(2)).max(1);
    let focus_line = selected_line.unwrap_or_else(|| lines.len().saturating_sub(1));
    let scroll = focus_line.saturating_sub(visible_height.saturating_sub(1)) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(" Run "),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn append_persisted_tool_details(
    lines: &mut Vec<Line<'static>>,
    messages: &[ProtocolMessage],
    state: &AppState,
) {
    for message in messages {
        if let Some(tool_calls) = &message.tool_calls {
            for call in tool_calls {
                lines.push(Line::styled(
                    format!("    ◇ {} · {}", call.function.name, call.id),
                    muted(state),
                ));
                append_limited_lines(lines, &call.function.arguments, 10, muted(state), "      ");
            }
        }
        if message.role == agent_protocol::Role::Tool {
            lines.push(Line::styled(
                format!(
                    "    ↳ 结果 {}",
                    message.tool_call_id.as_deref().unwrap_or("unknown")
                ),
                muted(state),
            ));
            if let Some(content) = message.content.as_deref() {
                let pretty = serde_json::from_str::<serde_json::Value>(content)
                    .ok()
                    .and_then(|value| serde_json::to_string_pretty(&value).ok())
                    .unwrap_or_else(|| content.to_string());
                append_limited_lines(lines, &pretty, 14, muted(state), "      ");
            }
        }
    }
}

fn append_limited_lines(
    output: &mut Vec<Line<'static>>,
    value: &str,
    limit: usize,
    style: Style,
    prefix: &str,
) {
    let sanitized = sanitize_terminal_text(value);
    let mut count = 0;
    for line in sanitized.lines().take(limit) {
        output.push(Line::styled(format!("{prefix}{line}"), style));
        count += 1;
    }
    if sanitized.lines().count() > count {
        output.push(Line::styled(format!("{prefix}…内容已截断"), style));
    }
}

fn render_subagents(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let agents = state
        .active_view()
        .map(|view| view.subagents.values().collect::<Vec<_>>())
        .unwrap_or_default();
    let items = if agents.is_empty() {
        vec![ListItem::new(Line::styled("暂无 Subagent", muted(state)))]
    } else {
        agents
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                let mut lines = vec![Line::styled(
                    format!(
                        "{} {} · {} · {}",
                        selection_marker(index, state.selected_inspector),
                        agent.identity.name,
                        agent.role.as_str(),
                        subagent_status(agent.status)
                    ),
                    if index == state.selected_inspector {
                        selected_style(state)
                    } else if agent.status.is_active() {
                        accent(state)
                    } else {
                        Style::default()
                    },
                )];
                if let Some(task) = &agent.latest_task {
                    lines.push(Line::styled(format!("  {task}"), muted(state)));
                }
                if let Some(summary) = &agent.latest_summary {
                    lines.push(Line::styled(
                        format!(
                            "  {} 次模型 · {} 次工具 · {}",
                            summary.model_calls,
                            summary.tool_calls,
                            subagent_run_status(summary.status)
                        ),
                        muted(state),
                    ));
                    for change in summary.file_changes.iter().take(4) {
                        lines.push(Line::styled(
                            format!("    {} {}", change.operation.as_str(), change.path),
                            muted(state),
                        ));
                    }
                    for shell in summary.shell_commands.iter().take(3) {
                        lines.push(Line::styled(
                            format!("    $ {} → {:?}", shell.command, shell.exit_code),
                            muted(state),
                        ));
                    }
                    if let Some(error) = &summary.error {
                        lines.push(Line::styled(format!("    {error}"), error_style(state)));
                    }
                }
                if index == state.selected_inspector {
                    if let Some(transcript) = state.subagent_transcripts.get(&agent.id) {
                        lines.push(Line::styled("  Transcript", emphasis(state)));
                        let start = transcript.lines.len().saturating_sub(24);
                        for transcript_line in &transcript.lines[start..] {
                            lines.push(Line::styled(
                                format!("    {}", sanitize_terminal_text(transcript_line)),
                                muted(state),
                            ));
                        }
                        if start > 0 {
                            lines.push(Line::styled(
                                format!("    …省略较早的 {start} 行"),
                                muted(state),
                            ));
                        }
                    } else {
                        lines.push(Line::styled("  Enter 加载 transcript", muted(state)));
                    }
                }
                ListItem::new(lines)
            })
            .collect()
    };
    let mut list_state = ListState::default().with_selected(
        (!agents.is_empty()).then_some(state.selected_inspector.min(agents.len() - 1)),
    );
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" Subagent "),
        ),
        area,
        &mut list_state,
    );
}

fn render_settings(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let titles = ["模型", "MCP", "命令", "Subagent"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(settings_index(state.settings.section))
            .highlight_style(selected_style(state))
            .divider(" │ ")
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(" Morrow 托管设置 "),
            ),
        rows[0],
    );

    if state.settings.loading && state.settings.snapshot.is_none() {
        frame.render_widget(
            Paragraph::new(format!(
                "{} 正在加载设置…",
                SPINNER[state.spinner % SPINNER.len()]
            ))
            .style(muted(state))
            .block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
            rows[1],
        );
        return;
    }
    if let Some(error) = &state.settings.error {
        frame.render_widget(
            Paragraph::new(format!("设置加载失败: {error}"))
                .style(error_style(state))
                .block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
            rows[1],
        );
        return;
    }
    let Some(settings) = state.settings.snapshot.as_ref() else {
        frame.render_widget(
            Paragraph::new("设置尚未加载，按 Ctrl+, 重试。")
                .style(muted(state))
                .block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
            rows[1],
        );
        return;
    };
    match state.settings.section {
        SettingsSection::Models => render_model_settings(frame, rows[1], state, settings),
        SettingsSection::Mcp => render_mcp_settings(frame, rows[1], state, settings),
        SettingsSection::Commands => render_command_settings(frame, rows[1], state, settings),
        SettingsSection::Subagents => render_subagent_settings(frame, rows[1], state, settings),
    }
}

fn render_model_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    settings: &SettingsSnapshot,
) {
    let mut items = Vec::new();
    if settings.providers.is_empty() {
        items.push(ListItem::new(vec![
            Line::styled("尚未配置模型供应商", emphasis(state)),
            Line::from("按 n 添加供应商；填写 Base URL 和 API Key 后可发现模型。"),
            Line::styled(
                "密钥只写入托管配置，界面和状态文件均不会显示明文。",
                muted(state),
            ),
        ]));
    } else {
        for (index, provider) in settings.providers.iter().enumerate() {
            let selected = index == state.settings.selected;
            let mut lines = vec![
                Line::styled(
                    format!(
                        "{} {} · {}{}",
                        selection_marker(index, state.settings.selected),
                        provider.name,
                        if provider.enabled {
                            "已启用"
                        } else {
                            "已停用"
                        },
                        if provider.read_only { " · 只读" } else { "" }
                    ),
                    if selected {
                        selected_style(state)
                    } else {
                        emphasis(state)
                    },
                ),
                Line::styled(
                    format!(
                        "  {} · {} · 超时 {}s",
                        provider.base_url, provider.api_format, provider.timeout_secs
                    ),
                    muted(state),
                ),
                Line::styled(
                    format!(
                        "  API Key: {} · {} 个模型",
                        if provider.api_key_configured {
                            "••••••••"
                        } else {
                            "未配置"
                        },
                        provider.models.len()
                    ),
                    muted(state),
                ),
            ];
            for model in &provider.models {
                lines.push(Line::styled(
                    format!(
                        "    {} ({}) · ctx {} / out {} · tools {} · {:?}",
                        model.name,
                        model.id,
                        model.context_window_tokens,
                        model.reserved_output_tokens,
                        if model.supports_tools { "yes" } else { "no" },
                        model.reasoning_profile
                    ),
                    muted(state),
                ));
            }
            items.push(ListItem::new(lines));
        }
    }
    let default = settings.default_model.as_ref().map_or_else(
        || "未设置".to_string(),
        |model| format!("{}/{}", model.provider_id, model.model_id),
    );
    items.push(ListItem::new(Line::styled(
        format!(
            "默认模型: {default} · 已发现 {} 个模型",
            settings.models.len()
        ),
        accent(state),
    )));
    for (model_index, model) in settings.models.iter().enumerate() {
        let index = settings.providers.len() + model_index;
        let is_default = settings.default_model.as_ref().is_some_and(|selection| {
            selection.provider_id == model.provider_id && selection.model_id == model.model_id
        });
        items.push(ListItem::new(Line::styled(
            format!(
                "{} {}{} · {}",
                selection_marker(index, state.settings.selected),
                if is_default { "★ " } else { "  " },
                model.label,
                if model.supports_reasoning {
                    "支持推理"
                } else {
                    "无推理"
                }
            ),
            if index == state.settings.selected {
                selected_style(state)
            } else if is_default {
                accent(state)
            } else {
                Style::default()
            },
        )));
    }
    let selected_row = if state.settings.selected >= settings.providers.len() {
        state.settings.selected.saturating_add(1)
    } else {
        state.settings.selected
    };
    let mut list_state = ListState::default()
        .with_selected((!items.is_empty()).then_some(selected_row.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" 模型供应商 ")
                .title_bottom(" n 新建 · e/Enter 编辑 · d 删除 · f 发现 · 模型上 Enter 设默认 "),
        ),
        area,
        &mut list_state,
    );
}

fn render_mcp_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    settings: &SettingsSnapshot,
) {
    let items = if settings.mcp_servers.is_empty() {
        vec![ListItem::new(vec![
            Line::styled("还没有 MCP server", emphasis(state)),
            Line::from("按 n 新建 stdio/HTTP server，或按 i 导入现有配置。"),
        ])]
    } else {
        settings
            .mcp_servers
            .iter()
            .enumerate()
            .map(|(index, server)| {
                let transport = match server.transport {
                    McpTransport::Stdio => "stdio",
                    McpTransport::Http => "HTTP",
                };
                let source = match server.source {
                    crate::backend::McpServerSource::RuntimeConfig => "morrow.toml",
                    crate::backend::McpServerSource::MorrowManaged => "Morrow 托管",
                };
                ListItem::new(vec![
                    Line::styled(
                        format!(
                            "{} {} · {} · {} · {}{}",
                            selection_marker(index, state.settings.selected),
                            server.name,
                            transport,
                            if server.enabled {
                                "已启用"
                            } else {
                                "已停用"
                            },
                            source,
                            if server.read_only { " · 只读" } else { "" }
                        ),
                        if index == state.settings.selected {
                            selected_style(state)
                        } else {
                            emphasis(state)
                        },
                    ),
                    Line::styled(format!("  {}", server.endpoint), muted(state)),
                    Line::styled(
                        format!(
                            "  args {:?} · cwd {} · timeout {}/{}s",
                            server.args,
                            server
                                .cwd
                                .as_ref()
                                .map_or("—".to_string(), |path| path.display().to_string()),
                            server.startup_timeout_secs,
                            server.tool_timeout_secs
                        ),
                        muted(state),
                    ),
                    Line::styled(
                        format!(
                            "  env keys [{}] · header keys [{}]",
                            server.env_keys.join(", "),
                            server.header_keys.join(", ")
                        ),
                        muted(state),
                    ),
                ])
            })
            .collect()
    };
    let mut list_state = ListState::default()
        .with_selected((!items.is_empty()).then_some(state.settings.selected.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" MCP servers ")
                .title_bottom(" n 新建 · i 导入 · e 编辑 · f 测试 · Space 启停 · d 删除 · editor Ctrl+T 测试草稿 "),
        ),
        area,
        &mut list_state,
    );
}

fn render_command_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    settings: &SettingsSnapshot,
) {
    let items = if settings.commands.is_empty() {
        vec![ListItem::new(vec![
            Line::styled("还没有托管命令", emphasis(state)),
            Line::from("按 n 创建；聊天输入 / 可补全命令。"),
        ])]
    } else {
        settings
            .commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                ListItem::new(vec![
                    Line::styled(
                        format!(
                            "{} /{} · {}",
                            selection_marker(index, state.settings.selected),
                            command.name,
                            command.description
                        ),
                        if index == state.settings.selected {
                            selected_style(state)
                        } else {
                            emphasis(state)
                        },
                    ),
                    Line::styled(
                        format!("  参数提示: {}", command.argument_hint),
                        muted(state),
                    ),
                    Line::styled(format!("  {}", command.prompt), muted(state)),
                ])
            })
            .collect()
    };
    let mut list_state = ListState::default()
        .with_selected((!items.is_empty()).then_some(state.settings.selected.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" 自定义命令 ")
                .title_bottom(" n 新建 · e 编辑 · d 删除 "),
        ),
        area,
        &mut list_state,
    );
}

fn render_subagent_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    settings: &SettingsSnapshot,
) {
    let columns = if area.width >= 90 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area)
    };
    let identities = if settings.subagent_identities.is_empty() {
        vec![ListItem::new(Line::styled("暂无身份", muted(state)))]
    } else {
        settings
            .subagent_identities
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                ListItem::new(vec![
                    Line::styled(
                        format!(
                            "{} {} · {}",
                            selection_marker(index, state.settings.selected),
                            identity.identity.name,
                            identity.identity.id
                        ),
                        if index == state.settings.selected {
                            selected_style(state)
                        } else {
                            Style::default()
                        },
                    ),
                    Line::styled(
                        if identity.avatar_configured {
                            "  已配置头像（编辑时可替换或移除）"
                        } else {
                            "  无头像"
                        },
                        muted(state),
                    ),
                ])
            })
            .collect()
    };
    let mut identity_state = ListState::default().with_selected(
        (state.settings.selected < settings.subagent_identities.len())
            .then_some(state.settings.selected),
    );
    frame.render_stateful_widget(
        List::new(identities).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" 身份 ")
                .title_bottom(" n 新建 · e 编辑/头像 · d 删除 · P 重置身份 "),
        ),
        columns[0],
        &mut identity_state,
    );

    let role_offset = settings.subagent_identities.len();
    let roles = settings
        .subagent_roles
        .iter()
        .enumerate()
        .map(|(role_index, role)| {
            let index = role_offset + role_index;
            let model = role.settings.model_selection.as_ref().map_or_else(
                || "继承默认模型".to_string(),
                |model| format!("{}/{}", model.provider_id, model.model_id),
            );
            ListItem::new(vec![
                Line::styled(
                    format!(
                        "{} {}",
                        selection_marker(index, state.settings.selected),
                        role.role.as_str()
                    ),
                    if index == state.settings.selected {
                        selected_style(state)
                    } else {
                        emphasis(state)
                    },
                ),
                Line::from(format!("  模型: {model}")),
                Line::from(format!(
                    "  超时: {}s · 工具轮次: {}",
                    role.settings.timeout_secs, role.settings.max_tool_rounds
                )),
                Line::styled(
                    if role.settings.prompt_suffix.is_empty() {
                        "  提示词后缀: 无".to_string()
                    } else {
                        format!("  提示词后缀: {}", role.settings.prompt_suffix)
                    },
                    muted(state),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let mut role_state = ListState::default().with_selected(
        (state.settings.selected >= role_offset)
            .then_some(state.settings.selected.saturating_sub(role_offset)),
    );
    frame.render_stateful_widget(
        List::new(roles).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" 角色设置 ")
                .title_bottom(" e/Enter 编辑角色 · R 重置角色 "),
        ),
        columns[1],
        &mut role_state,
    );
}

fn render_approval(
    frame: &mut Frame<'_>,
    root: Rect,
    state: &AppState,
    pending: &crate::state::PendingApproval,
) {
    let area = root;
    let mut lines = vec![
        Line::styled("工具请求需要批准", warning_style(state)),
        Line::from(format!(
            "会话: {}",
            session_label(state, &pending.session_id)
        )),
        Line::from(format!(
            "来源: {}",
            approval_origin(&pending.request.origin)
        )),
        Line::from(""),
        Line::styled("原因", emphasis(state)),
    ];
    for line in sanitize_terminal_text(&pending.request.reason).lines() {
        lines.push(Line::from(line.to_string()));
    }
    lines.push(Line::from(""));
    match &pending.request.action {
        ApprovalAction::ShellCommand {
            command,
            cwd,
            timeout_secs,
        } => {
            lines.push(Line::styled("Shell 命令", emphasis(state)));
            lines.push(Line::from(format!("$ {}", sanitize_terminal_text(command))));
            lines.push(Line::styled(
                format!(
                    "cwd: {}",
                    sanitize_terminal_text(&cwd.display().to_string())
                ),
                muted(state),
            ));
            lines.push(Line::styled(
                format!("timeout: {timeout_secs}s"),
                muted(state),
            ));
        }
        ApprovalAction::FileChanges { files, diff } => {
            lines.push(Line::styled("文件变更", emphasis(state)));
            for file in files {
                lines.push(Line::from(format!(
                    "{} {} ({} replacements)",
                    file.operation.as_str(),
                    sanitize_terminal_text(&file.path),
                    file.replacements
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled("Diff", emphasis(state)));
            for line in sanitize_terminal_text(diff).lines() {
                lines.push(Line::styled(line.to_string(), diff_style(state, line)));
            }
        }
    }
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .title(" 审批 ")
        .border_style(warning_style(state));
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let content_width = usize::from(rows[0].width.max(1));
    let content_height = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(content_width))
        .sum::<usize>();
    let max_scroll = content_height
        .saturating_sub(usize::from(rows[0].height))
        .min(usize::from(u16::MAX)) as u16;
    let scroll = state.approval_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "Y批准 N拒绝 · ↑↓/Pg滚动 {scroll}/{max_scroll} · Enter不批准 · 队列{}",
                state.approvals.len()
            ),
            warning_style(state),
        )),
        rows[1],
    );
}

fn render_overlay(frame: &mut Frame<'_>, root: Rect, state: &AppState, overlay: &Overlay) {
    match overlay {
        Overlay::Help => render_help(frame, root, state),
        Overlay::ActionPalette { selected } => render_palette(frame, root, state, *selected),
        Overlay::ExitConfirm => render_exit_confirm(frame, root, state),
        Overlay::ConfirmDelete(target) => {
            let (title, description, confirm) = match target {
                crate::state::DeleteTarget::ResetSubagentRoles => (
                    " 重置角色确认 ",
                    "将恢复所有 Subagent 角色覆盖设置。",
                    "Y 确认重置 · N / Esc 取消",
                ),
                crate::state::DeleteTarget::ResetSubagentProfiles => (
                    " 重置身份确认 ",
                    "将恢复内置 Subagent 身份；自定义身份和头像会被移除。",
                    "Y 确认重置 · N / Esc 取消",
                ),
                _ => (
                    " 删除确认 ",
                    "此操作不可撤销。",
                    "Y 确认删除 · N / Esc 取消",
                ),
            };
            let area = root;
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(description, error_style(state)),
                    Line::from(format!("目标: {target:?}")),
                    Line::from(""),
                    Line::styled(confirm, warning_style(state)),
                ])
                .block(
                    Block::default()
                        .borders(Borders::TOP | Borders::BOTTOM)
                        .title(title),
                )
                .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::SettingsEditor(editor) => render_settings_editor(frame, root, state, editor),
        Overlay::SubagentFollowUp { instance_id, value } => {
            let area = root;
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled(format!("Subagent: {instance_id}"), muted(state)),
                    Line::from(""),
                    Line::from(if value.is_empty() {
                        "输入 follow-up…_".to_string()
                    } else {
                        format!("{value}_")
                    }),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::TOP | Borders::BOTTOM)
                        .title(" Follow-up · Enter 发送 · Esc 取消 "),
                )
                .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn render_help(frame: &mut Frame<'_>, root: Rect, state: &AppState) {
    let area = root;
    let text = Text::from(vec![
        Line::styled("全局", emphasis(state)),
        Line::from("Ctrl+B 会话  Ctrl+G 检查器  Ctrl+, 设置（也可用 :settings）"),
        Line::from("Ctrl+P 动作面板  F2 模型  F3 推理  F4 权限  Ctrl+O 推理展开"),
        Line::from("Ctrl+Q 退出"),
        Line::from(""),
        Line::styled("输入", emphasis(state)),
        Line::from("Enter 提交  Ctrl+J / Alt+Enter 换行  Ctrl+C 取消运行或清空草稿"),
        Line::from("1 秒内再次按下 Ctrl+C：取消全部任务并强制退出"),
        Line::from("/ 托管命令补全  // 字面斜杠  @ 工作区路径补全"),
        Line::from(":settings  :sessions  :compact  :reset  :quit"),
        Line::from(""),
        Line::styled("运行与审批", emphasis(state)),
        Line::from("Y 复制所选纯文本；审批框中 Y 批准、N 拒绝，Enter 不会默认批准。"),
        Line::from("终端原生滚轮与选择用于 scrollback；PageUp/PageDown 浏览应用内记录。"),
        Line::from(""),
        Line::styled("F1 / Esc 关闭帮助", muted(state)),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(" 帮助 "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_palette(frame: &mut Frame<'_>, root: Rect, state: &AppState, selected: usize) {
    let actions = [
        "打开设置",
        "打开会话页",
        "切换模型",
        "切换推理级别",
        "切换权限",
        "压缩当前会话",
        "重置当前会话",
        "退出 Morrow",
    ];
    let items = actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            ListItem::new(format!("{} {action}", selection_marker(index, selected))).style(
                if index == selected {
                    selected_style(state)
                } else {
                    Style::default()
                },
            )
        })
        .collect::<Vec<_>>();
    let area = root;
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(" 动作 · Enter 执行 · Esc 关闭 "),
        ),
        area,
    );
}

fn render_exit_confirm(frame: &mut Frame<'_>, root: Rect, state: &AppState) {
    let active = state.has_active_work();
    let lines = if active {
        vec![
            Line::styled("仍有活动任务", warning_style(state)),
            Line::from("退出进程会终止所有任务；一期不包含后台 daemon。"),
            Line::from(""),
            Line::from("R / Esc 返回"),
            Line::from("W 等待全部完成后退出"),
            Line::styled("X 取消全部并退出", error_style(state)),
        ]
    } else {
        vec![
            Line::from("确定退出 Morrow？"),
            Line::from(""),
            Line::from("Enter / Q 退出 · R / Esc 返回"),
        ]
    };
    let area = root;
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(" 退出 "),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_settings_editor(
    frame: &mut Frame<'_>,
    root: Rect,
    state: &AppState,
    editor: &crate::state::SettingsEditor,
) {
    let area = root;
    let mut lines = Vec::new();
    for (index, field) in editor.fields.iter().enumerate() {
        let marker = selection_marker(index, editor.selected);
        let value = if field.secret {
            if field.value.is_empty() {
                "（留空以保留旧密钥）".to_string()
            } else {
                "•".repeat(field.value.chars().count().clamp(6, 16))
            }
        } else if field.value.is_empty() {
            "（空）".to_string()
        } else {
            sanitize_terminal_text(&field.value)
        };
        lines.push(Line::styled(
            format!("{marker} {}", field.label),
            if index == editor.selected {
                selected_style(state)
            } else {
                emphasis(state)
            },
        ));
        lines.push(Line::from(format!("  {value}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "↑↓/Tab 字段 · Ctrl+U 清空 · Ctrl+J 换行 · Ctrl+S 保存 · MCP Ctrl+T 测试 · Esc 取消",
        muted(state),
    ));
    if let Some(status) = state
        .status
        .as_deref()
        .filter(|status| status.contains("错误"))
    {
        lines.push(Line::styled(status.to_string(), error_style(state)));
    }
    let visible_height = usize::from(area.height.saturating_sub(2)).max(1);
    let selected_line = editor.selected.saturating_mul(2);
    let scroll = selected_line.saturating_sub(visible_height.saturating_sub(3)) as u16;
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .title(format!(" {} ", editor.title)),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn session_label(state: &AppState, session_id: &str) -> String {
    state
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .map_or_else(
            || session_id.to_string(),
            |session| format!("{} ({})", session.title, session.id),
        )
}

fn approval_origin(origin: &ApprovalOrigin) -> String {
    match origin {
        ApprovalOrigin::Unknown => "未知来源".to_string(),
        ApprovalOrigin::ParentTurn { turn_id, .. } => turn_id.as_ref().map_or_else(
            || "主会话 turn".to_string(),
            |id| format!("主会话 turn {id}"),
        ),
        ApprovalOrigin::SubagentRun {
            instance_id,
            role,
            identity_name,
            ..
        } => format!(
            "Subagent {} · {} · {}",
            identity_name.as_deref().unwrap_or(instance_id),
            role.as_str(),
            instance_id
        ),
    }
}

fn settings_index(section: SettingsSection) -> usize {
    match section {
        SettingsSection::Models => 0,
        SettingsSection::Mcp => 1,
        SettingsSection::Commands => 2,
        SettingsSection::Subagents => 3,
    }
}

fn status_label(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "运行中",
        TurnStatus::Completed => "完成",
        TurnStatus::Failed => "失败",
    }
}

fn turn_status_marker(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "◌",
        TurnStatus::Completed => "✓",
        TurnStatus::Failed => "×",
    }
}

fn subagent_status(status: SubagentInstanceStatus) -> &'static str {
    match status {
        SubagentInstanceStatus::Idle => "空闲",
        SubagentInstanceStatus::Queued => "排队",
        SubagentInstanceStatus::Running => "运行中",
        SubagentInstanceStatus::WaitingApproval => "等待审批",
        SubagentInstanceStatus::Interrupted => "已中断",
        SubagentInstanceStatus::Failed => "失败",
        SubagentInstanceStatus::Cancelled => "已取消",
    }
}

fn subagent_run_status(status: agent_protocol::SubagentRunStatus) -> &'static str {
    use agent_protocol::SubagentRunStatus;
    match status {
        SubagentRunStatus::Queued => "排队",
        SubagentRunStatus::Running => "运行中",
        SubagentRunStatus::WaitingApproval => "等待审批",
        SubagentRunStatus::Completed => "完成",
        SubagentRunStatus::Failed => "失败",
        SubagentRunStatus::Cancelled => "已取消",
        SubagentRunStatus::Interrupted => "已中断",
    }
}

fn compact_number(value: usize) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn wrapped_text_height(text: &Text<'_>, width: u16) -> u16 {
    let height = text.lines.iter().fold(0usize, |height, line| {
        height.saturating_add(usize::from(wrapped_line_height(line, width)))
    });
    height.min(usize::from(u16::MAX)) as u16
}

fn wrapped_line_height(line: &Line<'_>, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    (line.width().max(1).saturating_sub(1) / width + 1).min(usize::from(u16::MAX)) as u16
}

fn selection_marker(index: usize, selected: usize) -> &'static str {
    if index == selected { "›" } else { " " }
}

fn push_plain(text: &mut Text<'static>, value: &str, style: Style) {
    let sanitized = sanitize_terminal_text(value);
    if sanitized.is_empty() {
        text.lines.push(Line::styled("", style));
        return;
    }
    text.lines.extend(
        sanitized
            .lines()
            .map(|line| Line::styled(line.to_string(), style)),
    );
}

fn truncate_to_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut output = String::new();
    for character in value.chars() {
        let mut buffer = [0; 4];
        let character_width = UnicodeWidthStr::width(character.encode_utf8(&mut buffer));
        if UnicodeWidthStr::width(output.as_str()) + character_width + 1 > width {
            break;
        }
        output.push(character);
    }
    output.push('…');
    output
}

fn diff_style(state: &AppState, line: &str) -> Style {
    if line.starts_with('+') && !line.starts_with("+++") {
        theme::success(state.no_color)
    } else if line.starts_with('-') && !line.starts_with("---") {
        theme::error(state.no_color)
    } else if line.starts_with("@@") {
        theme::info(state.no_color)
    } else {
        Style::default()
    }
}

fn status_style(state: &AppState, status: TurnStatus) -> Style {
    match status {
        TurnStatus::Running => accent(state),
        TurnStatus::Completed => theme::success(state.no_color),
        TurnStatus::Failed => error_style(state),
    }
}

fn emphasis(state: &AppState) -> Style {
    theme::emphasis(state.no_color)
}

fn accent(state: &AppState) -> Style {
    theme::accent(state.no_color)
}

fn muted(state: &AppState) -> Style {
    theme::muted(state.no_color)
}

fn warning_style(state: &AppState) -> Style {
    theme::warning(state.no_color)
}

fn error_style(state: &AppState) -> Style {
    theme::error(state.no_color)
}

fn selected_style(state: &AppState) -> Style {
    theme::selected(state.no_color)
}

#[cfg(test)]
mod tests {
    use agent_protocol::{
        ApprovalRequest, Message, PermissionProfile, Session, ShellCommandSummary, ToolCall,
        ToolExecutionSummary, Turn, TurnRecord, TurnStep,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;
    use crate::backend::{SessionInfo, SessionSnapshot, WorkspaceSnapshot};
    use crate::persistence::WorkspaceTuiState;

    fn test_state(width: u16, height: u16) -> AppState {
        let mut session = Session::new();
        let user = Message::user("你好 👋");
        let mut turn = Turn::running(user.clone());
        turn.complete(Message::assistant("# 回答\n\n```rust\nfn main() {}\n```"));
        session.apply_turn(TurnRecord::new(
            turn,
            vec![user, Message::assistant("完成")],
        ));
        let info = SessionInfo {
            id: "work".to_string(),
            title: "工作会话".to_string(),
            archived: false,
            running: false,
            model: None,
            permissions: PermissionProfile::default(),
        };
        let snapshot = WorkspaceSnapshot {
            sessions: vec![info.clone()],
            active_session: Some(SessionSnapshot {
                info,
                session,
                subagents: Vec::new(),
                approvals: Vec::new(),
            }),
            models: Vec::new(),
        };
        let mut state = AppState::new(
            std::path::PathBuf::from("/tmp/示例-workspace"),
            snapshot,
            Some(WorkspaceTuiState::default()),
            None,
            false,
        );
        state.page = MainPage::Chat;
        state.terminal_size = (width, height);
        state
    }

    fn draw(width: u16, height: u16, state: &mut AppState) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, state);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn responsive_layouts_render_cjk_and_emoji_without_panics() {
        for (width, height) in [(160, 40), (112, 36), (80, 24), (48, 12), (40, 10)] {
            let mut state = test_state(width, height);
            let screen = draw(width, height, &mut state);
            assert!(!screen.is_empty());
            assert!(screen.contains("workspace") || screen.contains("Morrow"));
        }
    }

    #[test]
    fn empty_session_renders_welcome_card_input_and_status_bar() {
        let mut state = test_state(112, 36);
        state
            .active_view_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .session = Session::new();

        let screen = draw(112, 36, &mut state);
        assert!(screen.contains("Morrow"), "{screen:?}");
        assert!(screen.contains(env!("CARGO_PKG_VERSION")), "{screen:?}");
        assert!(screen.contains("context"), "{screen:?}");
        assert!(screen.contains('›'), "{screen:?}");
    }

    #[test]
    fn chat_remains_single_column_even_when_legacy_panels_are_visible() {
        let mut state = test_state(160, 40);
        state.sessions_visible = true;
        state.inspector_visible = true;
        state
            .active_view_mut()
            .unwrap()
            .live
            .tools
            .push(crate::state::ToolRun {
                id: "tool".to_string(),
                name: "read_file".to_string(),
                status: TurnStatus::Running,
                summary: None,
                result: Some("README.md".to_string()),
            });

        let screen = draw(160, 40, &mut state);
        assert!(screen.contains("read_file"), "{screen:?}");
        assert!(!screen.contains("会话栏"), "{screen:?}");
    }

    #[test]
    fn bottom_panel_sections_render_at_reference_sizes() {
        for (width, height) in [(160, 40), (112, 36), (80, 24), (48, 12)] {
            for page in [MainPage::Sessions, MainPage::Inspector, MainPage::Settings] {
                let mut state = test_state(width, height);
                state.page = page;
                let screen = draw(width, height, &mut state);
                assert!(!screen.is_empty());
                match page {
                    MainPage::Sessions => assert!(screen.contains("work"), "{screen:?}"),
                    MainPage::Inspector => {
                        assert!(screen.contains("Run"), "{screen:?}");
                        assert!(screen.contains("Subagent"), "{screen:?}");
                    }
                    MainPage::Settings => assert!(screen.contains("Subagent"), "{screen:?}"),
                    MainPage::Chat => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn generated_session_title_is_not_repeated_as_its_id() {
        let mut state = test_state(112, 36);
        state.page = MainPage::Sessions;
        state.sessions[0].id = "session-123".to_string();
        state.sessions[0].title = "session-123".to_string();
        state.active_session_id = Some("session-123".to_string());

        let screen = draw(112, 36, &mut state);
        assert_eq!(screen.matches("session-123").count(), 2, "{screen:?}");
    }

    #[test]
    fn composer_layout_keeps_combining_and_zwj_graphemes_together() {
        let combining = "e\u{301}x";
        let combining_layout = composer_layout(combining, combining.len(), 3);
        assert_eq!(combining_layout.lines, [combining]);
        assert_eq!(
            (combining_layout.cursor_row, combining_layout.cursor_column),
            (0, 2)
        );

        let family = "👨‍👩‍👧‍👦x";
        let family_layout = composer_layout(family, family.len(), 4);
        assert_eq!(family_layout.lines, [family]);
        assert_eq!(
            (family_layout.cursor_row, family_layout.cursor_column),
            (0, 3)
        );
    }

    #[test]
    fn composer_layout_soft_wraps_long_lines_and_tracks_the_visual_cursor() {
        let layout = composer_layout("abcdefghij", 10, 4);
        assert_eq!(layout.lines, ["abcd", "efgh", "ij"]);
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 2));
        assert_eq!(layout.cursor_row.saturating_add(1).saturating_sub(2), 1);
    }

    #[test]
    fn overlay_replaces_the_bottom_panel_without_hiding_the_transcript() {
        let mut state = test_state(80, 24);
        state.overlay = Some(Overlay::Help);
        let screen = draw(80, 24, &mut state);
        assert!(screen.contains("workspace"), "{screen:?}");
        assert!(screen.contains("scrollback"), "{screen:?}");
    }

    #[test]
    fn durable_history_above_the_viewport_is_exposed_for_native_scrollback() {
        let mut state = test_state(80, 18);
        let mut session = Session::new();
        for index in 0..20 {
            let user = Message::user(format!("history user {index}"));
            let assistant = Message::assistant(format!("history assistant {index}"));
            let mut turn = Turn::running(user.clone());
            turn.complete(assistant.clone());
            session.apply_turn(TurnRecord::new(turn, vec![user, assistant]));
        }
        state
            .active_view_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .session = session;
        state.active_view_mut().unwrap().live.text = "mutable streaming tail".repeat(20);
        let backend = TestBackend::new(80, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut inline = None;
        terminal
            .draw(|frame| {
                inline = Some(render_inline(frame, &mut state));
            })
            .unwrap();

        let scrollback = inline.unwrap().scrollback.unwrap();
        let text = scrollback
            .rows
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(scrollback.rows.area.height > 0);
        assert!(text.contains("history user 0"), "{text:?}");
        assert!(!text.contains("mutable streaming tail"), "{text:?}");
    }

    #[test]
    fn exit_render_removes_transient_composer_and_footer() {
        let mut state = test_state(80, 24);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_exit(frame, &mut state);
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!screen.contains("Enter 发送"), "{screen:?}");
        assert!(!screen.contains("context"), "{screen:?}");
        assert!(screen.contains('▌'), "{screen:?}");
    }

    #[test]
    fn approval_with_long_diff_stays_inside_test_backend() {
        let mut state = test_state(92, 26);
        state.approvals.push_back(crate::state::PendingApproval {
            session_id: "work".to_string(),
            request: ApprovalRequest::file_changes(
                "approval-1",
                Vec::new(),
                (0..200)
                    .map(|index| format!("+ diff-line-{index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                "需要写入文件",
            ),
        });
        let first_page = draw(92, 26, &mut state);
        assert!(first_page.contains("diff-line-0"), "{first_page:?}");
        assert!(first_page.contains("Enter"), "{first_page:?}");

        state.approval_scroll = u16::MAX;
        let last_page = draw(92, 26, &mut state);
        assert!(last_page.contains("diff-line-199"), "{last_page:?}");
        assert!(!last_page.contains("diff-line-0"), "{last_page:?}");
    }

    #[test]
    fn all_overlay_kinds_render() {
        let overlays = [
            Overlay::Help,
            Overlay::ActionPalette { selected: 2 },
            Overlay::ExitConfirm,
            Overlay::SubagentFollowUp {
                instance_id: "agent-1".to_string(),
                value: "继续检查".to_string(),
            },
        ];
        for overlay in overlays {
            let mut state = test_state(80, 24);
            state.overlay = Some(overlay);
            assert!(!draw(80, 24, &mut state).is_empty());
        }
    }

    #[test]
    fn settings_editor_scrolls_selected_late_field_into_view() {
        let mut state = test_state(80, 24);
        state.overlay = Some(Overlay::SettingsEditor(crate::state::SettingsEditor {
            title: "MCP".to_string(),
            kind: crate::state::EditorKind::McpServer {
                original_name: None,
            },
            fields: (0..13)
                .map(|index| crate::state::FormField {
                    label: format!("field-{index}"),
                    value: format!("value-{index}"),
                    secret: false,
                })
                .collect(),
            selected: 12,
        }));
        let screen = draw(80, 24, &mut state);
        assert!(screen.contains("field-12"), "{screen:?}");
        assert!(screen.contains("value-12"), "{screen:?}");
    }

    #[test]
    fn persisted_tool_calls_reconstruct_semantic_cards() {
        let mut state = test_state(100, 40);
        let user = Message::user("inspect");
        let call = ToolCall::function("call-1", "read_file", r#"{"path":"README.md"}"#);
        let assistant_call = Message::assistant_tool_calls(vec![call]);
        let tool_result = Message::tool_result("call-1", r#"{"ok":true,"path":"README.md"}"#);
        let final_message = Message::assistant("done");
        let mut turn = Turn::running(user.clone());
        turn.steps[0].complete();
        let mut tool_step = TurnStep::running_tool_call("read_file", "call-1");
        tool_step.complete();
        turn.steps.push(tool_step);
        turn.complete(final_message.clone());
        let mut session = Session::new();
        session.apply_turn(TurnRecord::new(
            turn,
            vec![user, assistant_call, tool_result, final_message],
        ));
        state
            .active_view_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .session = session;

        let screen = draw(100, 40, &mut state);
        assert!(screen.contains("read_file"), "{screen:?}");
        assert!(screen.contains("README.md"), "{screen:?}");
        assert!(screen.contains("done"), "{screen:?}");
    }

    #[test]
    fn malformed_tool_call_ids_render_safe_generic_cards() {
        let mut state = test_state(100, 60);
        let user = Message::user("inspect malformed history");
        let assistant_call = Message::assistant_tool_calls(vec![
            ToolCall::function("duplicate", "read_file", r#"{"path":"one"}"#),
            ToolCall::function("duplicate", "read_file", r#"{"path":"two"}"#),
        ]);
        let first_result = Message::tool_result("duplicate", "duplicate-result-one");
        let second_result = Message::tool_result("duplicate", "duplicate-result-two");
        let mut missing_id = Message::tool_result("missing", "missing-id-result");
        missing_id.tool_call_id = None;
        let final_message = Message::assistant("done");
        let mut turn = Turn::running(user.clone());
        turn.steps[0].complete();
        turn.steps
            .push(TurnStep::running_tool_call("read_file", "duplicate"));
        turn.steps
            .push(TurnStep::running_tool_call("read_file", "duplicate"));
        turn.complete(final_message.clone());
        let mut session = Session::new();
        session.apply_turn(TurnRecord::new(
            turn,
            vec![
                user,
                assistant_call,
                first_result,
                second_result,
                missing_id,
                final_message,
            ],
        ));
        state
            .active_view_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .session = session;

        let screen = draw(100, 60, &mut state);
        assert!(screen.contains("tool_call_id"), "{screen:?}");
        assert!(screen.contains("duplicate-result-one"), "{screen:?}");
        assert!(screen.contains("duplicate-result-two"), "{screen:?}");
        assert!(screen.contains("missing-id-result"), "{screen:?}");
    }

    #[test]
    fn tool_cards_use_pending_success_and_error_backgrounds() {
        let mut state = test_state(100, 60);
        state.active_view_mut().unwrap().live.tools.extend([
            crate::state::ToolRun {
                id: "pending".to_string(),
                name: "pending_tool".to_string(),
                status: TurnStatus::Running,
                summary: None,
                result: None,
            },
            crate::state::ToolRun {
                id: "success".to_string(),
                name: "shell".to_string(),
                status: TurnStatus::Completed,
                summary: Some(ToolExecutionSummary::shell(ShellCommandSummary {
                    command: "cargo check".to_string(),
                    exit_code: Some(0),
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })),
                result: None,
            },
            crate::state::ToolRun {
                id: "error".to_string(),
                name: "failed_tool".to_string(),
                status: TurnStatus::Failed,
                summary: Some(ToolExecutionSummary::error("boom")),
                result: None,
            },
        ]);
        let backend = TestBackend::new(100, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &mut state);
            })
            .unwrap();
        let cells = terminal.backend().buffer().content();
        assert!(
            cells
                .iter()
                .any(|cell| cell.bg == theme::PENDING_BACKGROUND)
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.bg == theme::SUCCESS_BACKGROUND)
        );
        assert!(cells.iter().any(|cell| cell.bg == theme::ERROR_BACKGROUND));
    }

    #[test]
    fn no_color_mode_does_not_emit_palette_colors() {
        let mut state = test_state(80, 24);
        state.no_color = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &mut state);
            })
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| { matches!(cell.fg, Color::Reset) && matches!(cell.bg, Color::Reset) })
        );
    }
}
