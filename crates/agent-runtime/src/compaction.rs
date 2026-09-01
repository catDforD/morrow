use super::*;

const REQUIRED_SUMMARY_SECTIONS: [&str; 7] = [
    "User Goals and Constraints",
    "Important Decisions",
    "Files and Code State",
    "Commands, Results, and Errors",
    "Current Progress",
    "Pending Tasks",
    "Open Questions",
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    Changed,
    Noop,
}

pub(crate) fn truncate_chars(value: String, max_chars: usize) -> (String, bool) {
    if value.chars().count() <= max_chars {
        return (value, false);
    }
    (value.chars().take(max_chars).collect(), true)
}

pub async fn maybe_auto_compact(
    client: &dyn Model,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    prompt: &str,
) -> Result<(), RuntimeError> {
    maybe_auto_compact_with_tools(
        client,
        system_prompt,
        session,
        context_config,
        model_limits,
        prompt,
        &[],
    )
    .await
}

pub async fn maybe_auto_compact_with_tools(
    client: &dyn Model,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    prompt: &str,
    tools: &[ToolDefinition],
) -> Result<(), RuntimeError> {
    if !context_config.auto_compact {
        return Ok(());
    }

    let budget = auto_compact_trigger_tokens(model_limits, context_config);
    let estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if estimate <= budget {
        return Ok(());
    }

    compact_session(client, session, context_config).await?;

    let compacted_estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if compacted_estimate > budget {
        return Err(RuntimeError::AgentRun(format!(
            "context is still over token budget after compaction ({compacted_estimate} > {budget})"
        )));
    }

    Ok(())
}

// Compatibility shim for callers that still pass compaction fields separately.
#[allow(clippy::too_many_arguments)]
pub async fn maybe_auto_compact_with_tools_and_middleware(
    client: &dyn Model,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
    prompt: &str,
    tools: &[ToolDefinition],
    context: MiddlewareExecutionContext,
    middleware: &MiddlewareRegistry,
) -> Result<MiddlewareCompactionOutcome, RuntimeError> {
    maybe_auto_compact_with_middleware_context(
        session,
        MiddlewareCompactionContext {
            client,
            system_prompt,
            context_config,
            model_limits,
            prompt,
            tools,
            execution_context: context,
            registry: middleware,
        },
    )
    .await
}

pub async fn maybe_auto_compact_with_middleware_context(
    session: &mut Session,
    context: MiddlewareCompactionContext<'_>,
) -> Result<MiddlewareCompactionOutcome, RuntimeError> {
    let MiddlewareCompactionContext {
        client,
        system_prompt,
        context_config,
        model_limits,
        prompt,
        tools,
        execution_context,
        registry,
    } = context;
    if !context_config.auto_compact {
        return Ok(MiddlewareCompactionOutcome {
            outcome: CompactionOutcome::Noop,
            events: Vec::new(),
            additional_context: Vec::new(),
        });
    }
    let budget = auto_compact_trigger_tokens(model_limits, context_config);
    let estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if estimate <= budget {
        return Ok(MiddlewareCompactionOutcome {
            outcome: CompactionOutcome::Noop,
            events: Vec::new(),
            additional_context: Vec::new(),
        });
    }
    let pre = registry
        .runtime()
        .run_pre_compact(PreCompactInput {
            context: execution_context.clone(),
            cause: CompactionCause::Automatic,
            estimated_tokens: estimate,
            token_budget: Some(budget),
            current_summary: session.context.summary.clone(),
            summarized_turns: session.context.summarized_turns,
        })
        .await;
    let pre_cancelled = pre.cancelled;
    let pre_denied = pre.denied();
    let mut events = pre.events;
    if pre_cancelled {
        return Err(RuntimeError::AgentRun("operation cancelled".to_string()));
    }
    if pre_denied {
        return Ok(MiddlewareCompactionOutcome {
            outcome: CompactionOutcome::Noop,
            events,
            additional_context: Vec::new(),
        });
    }
    let previous_summary = session.context.summary.clone();
    let mut draft = session.clone();
    let outcome =
        compact_session_with_context(client, &mut draft, context_config, &pre.context).await?;
    let mut additional_context = Vec::new();
    if outcome == CompactionOutcome::Changed {
        let post = registry
            .runtime()
            .run_post_compact(PostCompactInput {
                context: execution_context,
                cause: CompactionCause::Automatic,
                previous_summary,
                summary: draft.context.summary.clone().unwrap_or_default(),
                summarized_turns: draft.context.summarized_turns,
            })
            .await;
        events.extend(post.events);
        if post.cancelled {
            return Err(RuntimeError::AgentRun("operation cancelled".to_string()));
        }
        if !post.fatal_errors.is_empty() {
            return Err(RuntimeError::AgentRun(format!(
                "post-compact middleware failed: {}",
                post.fatal_errors.join("; ")
            )));
        }
        additional_context = post.context;
        *session = draft;
    }
    let compacted_estimate = estimate_context_tokens(system_prompt, session, prompt, tools);
    if compacted_estimate > budget {
        return Err(RuntimeError::AgentRun(format!(
            "context is still over token budget after compaction ({compacted_estimate} > {budget})"
        )));
    }
    Ok(MiddlewareCompactionOutcome {
        outcome,
        events,
        additional_context,
    })
}

pub(crate) fn auto_compact_trigger_tokens(
    model_limits: ModelContextLimits,
    context_config: ContextConfig,
) -> usize {
    let input_window = model_limits
        .context_window_tokens
        .saturating_sub(model_limits.reserved_output_tokens);
    let window_trigger =
        ((input_window as f64) * f64::from(context_config.auto_compact_threshold)).floor() as usize;
    // 绝对上限封顶：即使模型窗口很大，水位也主动收敛到 max_context_tokens 以内。
    match context_config.max_context_tokens {
        Some(max) => window_trigger.min(max),
        None => window_trigger,
    }
}

/// turn 内护栏上限：跟随自动压缩预算；关闭 auto_compact 时不启用护栏。
pub(crate) fn mid_turn_context_token_limit(
    context_config: ContextConfig,
    model_limits: ModelContextLimits,
) -> Option<usize> {
    context_config
        .auto_compact
        .then(|| auto_compact_trigger_tokens(model_limits, context_config))
}

pub async fn compact_session(
    client: &dyn Model,
    session: &mut Session,
    context_config: ContextConfig,
) -> Result<CompactionOutcome, RuntimeError> {
    compact_session_with_context(client, session, context_config, &[]).await
}

pub async fn compact_session_with_middleware(
    client: &dyn Model,
    session: &mut Session,
    context_config: ContextConfig,
    context: MiddlewareExecutionContext,
    middleware: &MiddlewareRegistry,
) -> Result<MiddlewareCompactionOutcome, RuntimeError> {
    compact_session_with_middleware_audit(client, session, context_config, context, middleware)
        .await
        .map_err(|failure| failure.error)
}

pub async fn compact_session_with_middleware_audit(
    client: &dyn Model,
    session: &mut Session,
    context_config: ContextConfig,
    context: MiddlewareExecutionContext,
    middleware: &MiddlewareRegistry,
) -> Result<MiddlewareCompactionOutcome, MiddlewareCompactionError> {
    let pre = middleware
        .runtime()
        .run_pre_compact(PreCompactInput {
            context: context.clone(),
            cause: CompactionCause::Manual,
            estimated_tokens: 0,
            token_budget: None,
            current_summary: session.context.summary.clone(),
            summarized_turns: session.context.summarized_turns,
        })
        .await;
    let pre_cancelled = pre.cancelled;
    let pre_denied = pre.denied();
    let mut events = pre.events;
    if pre_cancelled {
        return Err(MiddlewareCompactionError {
            error: RuntimeError::AgentRun("operation cancelled".to_string()),
            events,
        });
    }
    if pre_denied {
        return Ok(MiddlewareCompactionOutcome {
            outcome: CompactionOutcome::Noop,
            events,
            additional_context: Vec::new(),
        });
    }
    let previous_summary = session.context.summary.clone();
    let mut draft = session.clone();
    let outcome = match compact_session_with_context(
        client,
        &mut draft,
        context_config,
        &pre.context,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => return Err(MiddlewareCompactionError { error, events }),
    };
    if outcome == CompactionOutcome::Noop {
        return Ok(MiddlewareCompactionOutcome {
            outcome,
            events,
            additional_context: Vec::new(),
        });
    }
    let post = middleware
        .runtime()
        .run_post_compact(PostCompactInput {
            context,
            cause: CompactionCause::Manual,
            previous_summary,
            summary: draft.context.summary.clone().unwrap_or_default(),
            summarized_turns: draft.context.summarized_turns,
        })
        .await;
    events.extend(post.events);
    if post.cancelled {
        return Err(MiddlewareCompactionError {
            error: RuntimeError::AgentRun("operation cancelled".to_string()),
            events,
        });
    }
    if !post.fatal_errors.is_empty() {
        return Err(MiddlewareCompactionError {
            error: RuntimeError::AgentRun(format!(
                "post-compact middleware failed: {}",
                post.fatal_errors.join("; ")
            )),
            events,
        });
    }
    *session = draft;
    Ok(MiddlewareCompactionOutcome {
        outcome,
        events,
        additional_context: post.context,
    })
}

pub(crate) async fn compact_session_with_context(
    client: &dyn Model,
    session: &mut Session,
    context_config: ContextConfig,
    middleware_context: &[MiddlewareContextBlock],
) -> Result<CompactionOutcome, RuntimeError> {
    let prefix_len = compactable_prefix_len(session, context_config.retain_recent_turns);
    if prefix_len <= session.context.summarized_turns {
        return Ok(CompactionOutcome::Noop);
    }

    let records = session.turns[session.context.summarized_turns..prefix_len].to_vec();
    let summary = request_session_summary(
        client,
        session.context.summary.as_deref(),
        context_config.summary_target_tokens,
        context_config.compact_max_retries,
        &records,
        session.context.summarized_turns,
        middleware_context,
    )
    .await?;

    session.context.summary = Some(summary);
    session.context.summarized_turns = prefix_len;
    rebuild_active_thread(session);

    Ok(CompactionOutcome::Changed)
}

pub fn rebuild_active_thread(session: &mut Session) {
    let mut messages = Vec::new();
    if let Some(summary) = session.context.summary.as_ref() {
        messages.push(Message::system(format!("Session summary:\n{summary}")));
    }

    for record in session.turns.iter().skip(session.context.summarized_turns) {
        if record.turn.status == TurnStatus::Completed {
            messages.extend(record.messages.clone());
        }
    }

    session.active_thread.messages = messages;
}

fn compactable_prefix_len(session: &Session, retain_recent_turns: usize) -> usize {
    let completed_indices = session
        .turns
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            (record.turn.status == TurnStatus::Completed).then_some(index)
        })
        .collect::<Vec<_>>();

    if completed_indices.len() <= retain_recent_turns {
        return session.context.summarized_turns;
    }

    completed_indices[completed_indices.len() - retain_recent_turns]
        .max(session.context.summarized_turns)
}

async fn request_session_summary(
    client: &dyn Model,
    existing_summary: Option<&str>,
    target_tokens: usize,
    max_attempts: usize,
    records: &[TurnRecord],
    first_turn_index: usize,
    middleware_context: &[MiddlewareContextBlock],
) -> Result<String, RuntimeError> {
    let attempts = max_attempts.max(1);
    let mut repair_feedback = None;

    for _ in 0..attempts {
        let output = match request_raw_session_summary(
            client,
            existing_summary,
            target_tokens,
            repair_feedback.as_deref(),
            records,
            first_turn_index,
            middleware_context,
        )
        .await
        {
            Ok(output) => output,
            Err(_) => {
                return Ok(deterministic_session_summary(
                    existing_summary,
                    records,
                    first_turn_index,
                ));
            }
        };

        match parse_compact_summary_output(&output) {
            Ok(summary) => return Ok(summary),
            Err(error) => {
                repair_feedback = Some(error);
            }
        }
    }

    Ok(deterministic_session_summary(
        existing_summary,
        records,
        first_turn_index,
    ))
}

async fn request_raw_session_summary(
    client: &dyn Model,
    existing_summary: Option<&str>,
    target_tokens: usize,
    repair_feedback: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
    middleware_context: &[MiddlewareContextBlock],
) -> Result<String, RuntimeError> {
    let mut conversation = Conversation::with_system_prompt(
        "You compact long-running coding agent session history. Respond with text only. Do not call tools. Return one <analysis> block followed by one <summary> block.",
    );
    if !middleware_context.is_empty() {
        conversation.push(Message::system(render_middleware_context(
            middleware_context,
            "Additional middleware context for this compaction operation.",
        )));
    }
    conversation.push(Message::user(build_summary_prompt(
        existing_summary,
        target_tokens,
        repair_feedback,
        records,
        first_turn_index,
    )));

    let mut stream = client
        .stream(ModelRequest {
            conversation,
            tools: Vec::new(),
        })
        .await?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ModelEvent::ReasoningDelta(_) => {}
            ModelEvent::TextDelta(text) => output.push_str(&text),
            ModelEvent::Completed => {
                let output = output.trim().to_string();
                if output.is_empty() {
                    return Err(RuntimeError::AgentRun(
                        "summary model returned an empty summary".to_string(),
                    ));
                }
                return Ok(output);
            }
            ModelEvent::ToolCalls(_) => {
                return Err(RuntimeError::AgentRun(
                    "summary model requested tool calls".to_string(),
                ));
            }
        }
    }

    Err(RuntimeError::AgentRun(
        "summary model stream ended before completion".to_string(),
    ))
}

pub(crate) fn build_summary_prompt(
    existing_summary: Option<&str>,
    target_tokens: usize,
    repair_feedback: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(
        prompt,
        "Update the session summary. Target length: at most {target_tokens} tokens."
    );
    let _ = writeln!(
        prompt,
        "Output exactly one <analysis> block followed by one <summary> block."
    );
    let _ = writeln!(
        prompt,
        "The <summary> block must contain these section headings exactly:"
    );
    for section in REQUIRED_SUMMARY_SECTIONS {
        let _ = writeln!(prompt, "- {section}");
    }
    let _ = writeln!(prompt);
    let _ = writeln!(
        prompt,
        "Preserve user goals, constraints, decisions, file paths, code state, commands, results, errors, pending tasks, and open questions. Do not continue the conversation."
    );
    if let Some(feedback) = repair_feedback.filter(|feedback| !feedback.trim().is_empty()) {
        let _ = writeln!(prompt);
        let _ = writeln!(
            prompt,
            "Repair feedback from the previous invalid compact output:"
        );
        let _ = writeln!(prompt, "{feedback}");
    }
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Existing summary:");
    let _ = writeln!(prompt, "{}", existing_summary.unwrap_or("(none)"));
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Turns to incorporate:");

    for (offset, record) in records.iter().enumerate() {
        append_turn_record_transcript(&mut prompt, first_turn_index + offset, record);
    }

    prompt
}

fn append_turn_record_transcript(output: &mut String, index: usize, record: &TurnRecord) {
    let _ = writeln!(
        output,
        "\nTurn {index}: status={}",
        turn_status_label(record.turn.status)
    );
    if let Some(error) = record.turn.error.as_ref() {
        let _ = writeln!(output, "turn_error: {error}");
    }
    for message in &record.messages {
        let _ = writeln!(output, "{}:", message_role_label(message));
        if let Some(content) = message.content.as_ref() {
            let _ = writeln!(output, "{content}");
        }
        if let Some(tool_calls) = message.tool_calls.as_ref() {
            let tool_calls = serde_json::to_string(tool_calls).unwrap_or_else(|_| "[]".to_string());
            let _ = writeln!(output, "tool_calls: {tool_calls}");
        }
        if let Some(tool_call_id) = message.tool_call_id.as_ref() {
            let _ = writeln!(output, "tool_call_id: {tool_call_id}");
        }
    }
}

pub(crate) fn parse_compact_summary_output(output: &str) -> Result<String, String> {
    let normalized = strip_outer_markdown_code_fence(output);
    let summary = extract_xml_block(&normalized, "summary")?
        .ok_or_else(|| "compact response missing <summary> block".to_string())?;
    if summary.trim().is_empty() {
        return Err("compact summary response was empty".to_string());
    }
    if let Some(section) = REQUIRED_SUMMARY_SECTIONS
        .iter()
        .find(|section| !summary.contains(**section))
    {
        return Err(format!(
            "compact summary missing required section: {section}"
        ));
    }
    Ok(summary.trim().to_string())
}

fn extract_xml_block(content: &str, tag: &str) -> Result<Option<String>, String> {
    let Some((_open_start, open_end)) = find_opening_tag(content, tag) else {
        return Ok(None);
    };
    let Some((close_start, _close_end)) = find_closing_tag(&content[open_end..], tag) else {
        return Err(format!("compact response missing closing </{tag}> tag"));
    };
    let close_start = open_end + close_start;
    Ok(Some(content[open_end..close_start].trim().to_string()))
}

fn find_opening_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let mut start = 0;
    while let Some(relative) = lower[start..].find(&needle) {
        let tag_start = start + relative;
        let after = lower[tag_start + needle.len()..].chars().next();
        if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
            start = tag_start + needle.len();
            continue;
        }
        let tag_end = lower[tag_start..].find('>')? + tag_start + 1;
        return Some((tag_start, tag_end));
    }
    None
}

fn find_closing_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("</{tag}");
    let start = lower.find(&needle)?;
    let after = lower[start + needle.len()..].chars().next();
    if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
        return None;
    }
    let end = lower[start..].find('>')? + start + 1;
    Some((start, end))
}

fn strip_outer_markdown_code_fence(content: &str) -> String {
    let mut current = content.trim().to_string();
    loop {
        let stripped = strip_markdown_code_fence(&current);
        if stripped == current {
            return current;
        }
        current = stripped;
    }
}

fn strip_markdown_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let mut lines = trimmed.lines();
    let Some(first_line) = lines.next() else {
        return trimmed.to_string();
    };
    if !first_line.trim_start().starts_with("```") {
        return trimmed.to_string();
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim().to_string()
}

fn deterministic_session_summary(
    existing_summary: Option<&str>,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> String {
    let mut summary = String::new();
    let _ = writeln!(summary, "User Goals and Constraints");
    let _ = writeln!(
        summary,
        "- Previous summary: {}",
        existing_summary
            .map(|summary| truncate_summary_text(summary, 1_200))
            .unwrap_or_else(|| "(none)".to_string())
    );
    let _ = writeln!(
        summary,
        "- Compacted {} turn records with deterministic fallback.",
        records.len()
    );
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Important Decisions");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Files and Code State");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Commands, Results, and Errors");
    append_fallback_errors(&mut summary, records, first_turn_index);
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Current Progress");
    for (offset, record) in records.iter().enumerate().rev().take(6).rev() {
        let index = first_turn_index + offset;
        let _ = writeln!(
            summary,
            "- Turn {index}: status={}",
            turn_status_label(record.turn.status)
        );
        if let Some(content) = record.turn.user_message.content.as_ref() {
            let _ = writeln!(summary, "  user: {}", truncate_summary_text(content, 240));
        }
        if let Some(message) = record.turn.assistant_message.as_ref()
            && let Some(content) = message.content.as_ref()
        {
            let _ = writeln!(
                summary,
                "  assistant: {}",
                truncate_summary_text(content, 240)
            );
        }
    }
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Pending Tasks");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "Open Questions");
    let _ = writeln!(summary, "- (unknown from deterministic fallback)");

    summary.trim().to_string()
}

fn append_fallback_errors(output: &mut String, records: &[TurnRecord], first_turn_index: usize) {
    let mut wrote = false;
    for (offset, record) in records.iter().enumerate() {
        if let Some(error) = record.turn.error.as_ref() {
            let _ = writeln!(
                output,
                "- Turn {} error: {}",
                first_turn_index + offset,
                truncate_summary_text(error, 320)
            );
            wrote = true;
        }
    }
    if !wrote {
        let _ = writeln!(output, "- (none recorded)");
    }
}

fn truncate_summary_text(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.trim().to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

pub(crate) fn estimate_context_tokens(
    system_prompt: &str,
    session: &Session,
    prompt: &str,
    tools: &[ToolDefinition],
) -> usize {
    let tool_tokens = estimate_tool_definitions_tokens(tools);
    let raw_total = estimate_role_text_tokens(agent_protocol::Role::System, system_prompt)
        + estimate_role_text_tokens(agent_protocol::Role::User, prompt)
        + tool_tokens
        + session
            .active_thread
            .messages
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
    apply_request_padding(raw_total)
}

fn message_role_label(message: &Message) -> &'static str {
    match message.role {
        agent_protocol::Role::System => "system",
        agent_protocol::Role::User => "user",
        agent_protocol::Role::Assistant => "assistant",
        agent_protocol::Role::Tool => "tool",
    }
}

fn turn_status_label(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
    }
}
