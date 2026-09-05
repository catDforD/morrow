use super::*;
use serde_json::json;

#[test]
fn permission_mode_clamp_picks_the_more_restrictive_mode() {
    assert_eq!(
        PermissionMode::DangerFullAccess.clamp(PermissionMode::WorkspaceWrite),
        PermissionMode::WorkspaceWrite
    );
    assert_eq!(
        PermissionMode::ReadOnly.clamp(PermissionMode::DangerFullAccess),
        PermissionMode::ReadOnly
    );
    assert_eq!(
        PermissionMode::WorkspaceWrite.clamp(PermissionMode::WorkspaceWrite),
        PermissionMode::WorkspaceWrite
    );
    assert!(
        PermissionMode::ReadOnly.severity() < PermissionMode::WorkspaceWrite.severity()
            && PermissionMode::WorkspaceWrite.severity()
                < PermissionMode::DangerFullAccess.severity()
    );
}

#[test]
fn serializes_messages_in_openai_chat_shape() {
    let mut conversation = Conversation::with_system_prompt("You are helpful.");
    conversation.push(Message::user("Hello"));
    conversation.push(Message::assistant("Hi"));

    let value = serde_json::to_value(&conversation.messages).expect("serialize messages");

    assert_eq!(
        value,
        json!([
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ])
    );
}

#[test]
fn thread_serializes_long_term_messages_without_system_prompt() {
    let mut thread = Thread::new();
    thread.push(Message::user("Hello"));
    thread.push(Message::assistant("Hi"));

    let value = serde_json::to_value(&thread).expect("serialize thread");

    assert_eq!(
        value,
        json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ]
        })
    );
}

#[test]
fn thread_document_serializes_versioned_thread() {
    let mut thread = Thread::new();
    thread.push(Message::user("Hello"));
    thread.push(Message::assistant("Hi"));

    let document = ThreadDocument::new(thread.clone());
    let value = serde_json::to_value(&document).expect("serialize thread document");

    assert_eq!(
        value,
        json!({
            "schema_version": 2,
            "thread": {
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi"}
                ]
            }
        })
    );

    let decoded =
        serde_json::from_value::<ThreadDocument>(value).expect("deserialize thread document");

    assert_eq!(decoded.schema_version, THREAD_DOCUMENT_SCHEMA_VERSION);
    assert_eq!(decoded.thread, thread);
}

#[test]
fn session_document_serializes_versioned_session() {
    let mut active_thread = Thread::new();
    active_thread.push(Message::system("Session summary:\nKnown facts"));
    active_thread.push(Message::user("Continue"));
    let mut turn = Turn::running(Message::user("Hello"));
    turn.complete(Message::assistant("Hi"));
    let session = Session {
        active_thread: active_thread.clone(),
        turns: vec![TurnRecord::new(
            turn.clone(),
            vec![Message::user("Hello"), Message::assistant("Hi")],
        )],
        context: SessionContext {
            summary: Some("Known facts".to_string()),
            summarized_turns: 1,
        },
    };

    let document = SessionDocument::new(session.clone());
    let value = serde_json::to_value(&document).expect("serialize session document");

    assert_eq!(value["schema_version"], json!(7));
    assert_eq!(
        value["session"]["context"],
        json!({"summary": "Known facts", "summarized_turns": 1})
    );
    assert_eq!(
        value["session"]["active_thread"],
        serde_json::to_value(active_thread).expect("active thread")
    );

    let decoded =
        serde_json::from_value::<SessionDocument>(value).expect("deserialize session document");
    assert_eq!(decoded.schema_version, SESSION_DOCUMENT_SCHEMA_VERSION);
    assert_eq!(decoded.session, session);
}

#[test]
fn session_projection_serializes_required_empty_arrays() {
    let projection = SessionProjection {
        session_id: "session-1".to_string(),
        revision: 1,
        turns: vec![TurnProjection {
            id: "turn-1".to_string(),
            operation_id: "operation-1".to_string(),
            index: 0,
            status: SessionTurnStatus::Running,
            user_message: Message::user("Hello"),
            model: ModelInvocation {
                provider_id: "test".to_string(),
                provider_name: "Test".to_string(),
                model_id: "test-model".to_string(),
                model_name: "Test model".to_string(),
                reasoning: ReasoningLevel::Off,
            },
            permissions: PermissionProfile::default(),
            messages: vec![Message::user("Hello")],
            steps: Vec::new(),
            notices: Vec::new(),
            error: None,
            started_at_ms: 1,
            completed_at_ms: None,
        }],
        context: ModelContextProjection::default(),
        middleware_audit: Vec::new(),
        diagnostics: Vec::new(),
    };

    let value = serde_json::to_value(projection).expect("serialize session projection");

    assert_eq!(value["diagnostics"], json!([]));
    assert_eq!(value["turns"][0]["notices"], json!([]));
}

#[test]
fn applying_completed_turn_updates_active_thread_and_history_once() {
    let mut session = Session::from_thread(Thread {
        messages: vec![Message::user("Previous"), Message::assistant("Context")],
    });
    let user_message = Message::user("Hello");
    let assistant_message = Message::assistant("Hi");
    let mut turn = Turn::running(user_message.clone());
    turn.complete(assistant_message.clone());
    let record = TurnRecord::new(turn, vec![user_message.clone(), assistant_message.clone()]);

    session.apply_turn(record.clone());

    assert_eq!(
        session.active_thread.messages,
        vec![
            Message::user("Previous"),
            Message::assistant("Context"),
            user_message,
            assistant_message,
        ]
    );
    assert_eq!(session.turns, vec![record]);
}

#[test]
fn applying_failed_turn_updates_history_without_changing_active_thread() {
    let initial_thread = Thread {
        messages: vec![Message::user("Previous"), Message::assistant("Context")],
    };
    let mut session = Session::from_thread(initial_thread.clone());
    let record = TurnRecord::failed_user_prompt("Broken", "model error");

    session.apply_turn(record.clone());

    assert_eq!(session.active_thread, initial_thread);
    assert_eq!(session.turns, vec![record]);
}

#[test]
fn running_turn_cannot_be_applied_to_session() {
    let mut session = Session::new();
    let user_message = Message::user("Still running");
    let record = TurnRecord::new(Turn::running(user_message.clone()), vec![user_message]);

    let error = session
        .try_apply_turn(record)
        .expect_err("running turn must be rejected");

    assert_eq!(error, SessionApplyError);
    assert!(session.turns.is_empty());
    assert!(session.active_thread.messages.is_empty());
}

#[test]
fn permission_profile_defaults_shell_policy_by_mode() {
    assert_eq!(
        PermissionProfile::default(),
        PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Prompt,
        }
    );
    assert_eq!(
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite).shell,
        ShellPolicy::Prompt
    );
    assert_eq!(
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess).shell,
        ShellPolicy::Allow
    );
}

#[test]
fn serializes_approval_events() {
    let request = ApprovalRequest::shell_command(
        "approval-call_1",
        "cargo test",
        "/repo",
        30,
        "shell command requires approval",
    );
    let decision = ApprovalDecision::approve("approval-call_1");
    let events = vec![
        AgentEvent::ApprovalRequested(request),
        AgentEvent::ApprovalResolved(decision),
    ];

    let value = serde_json::to_value(&events).expect("serialize approval events");

    assert_eq!(
        value,
        json!([
            {
                "type": "approval_requested",
                "data": {
                    "id": "approval-call_1",
                    "action": {
                        "kind": "shell_command",
                        "command": "cargo test",
                        "cwd": "/repo",
                        "timeout_secs": 30
                    },
                    "reason": "shell command requires approval"
                }
            },
            {
                "type": "approval_resolved",
                "data": {
                    "request_id": "approval-call_1",
                    "approved": true
                }
            }
        ])
    );
}

#[test]
fn mcp_tool_approval_roundtrips_and_truncates_arguments() {
    let request = ApprovalRequest::mcp_tool(
        "approval-call_1",
        "docs",
        "search",
        r#"{"query":"morrow"}"#,
        "MCP tool requires approval",
    );

    let value = serde_json::to_value(&request).expect("serialize mcp approval");
    assert_eq!(
        value,
        json!({
            "id": "approval-call_1",
            "action": {
                "kind": "mcp_tool",
                "server": "docs",
                "tool": "search",
                "arguments": "{\"query\":\"morrow\"}"
            },
            "reason": "MCP tool requires approval"
        })
    );
    let parsed: ApprovalRequest = serde_json::from_value(value).expect("deserialize mcp approval");
    assert_eq!(parsed, request);

    let long = "x".repeat(MCP_ARGUMENTS_MAX_BYTES + 100);
    let truncated = ApprovalRequest::mcp_tool("approval-call_2", "docs", "search", long, "r");
    let ApprovalAction::McpTool { arguments, .. } = &truncated.action else {
        panic!("expected mcp tool action");
    };
    assert!(arguments.ends_with("…(truncated)"));
    assert!(arguments.len() <= MCP_ARGUMENTS_MAX_BYTES + "…(truncated)".len());

    // 多字节字符跨越截断边界时回退到字符边界，不产生非法 UTF-8。
    let boundary = format!("{}界", "a".repeat(MCP_ARGUMENTS_MAX_BYTES - 1));
    let truncated = ApprovalRequest::mcp_tool("approval-call_3", "docs", "search", boundary, "r");
    let ApprovalAction::McpTool { arguments, .. } = &truncated.action else {
        panic!("expected mcp tool action");
    };
    assert!(arguments.starts_with(&"a".repeat(MCP_ARGUMENTS_MAX_BYTES - 1)));
}

#[test]
fn serializes_warning_event() {
    let event = AgentEvent::Warning("mcp server docs: failed to start".to_string());

    let value = serde_json::to_value(&event).expect("serialize warning event");

    assert_eq!(
        value,
        json!({
            "type": "warning",
            "data": "mcp server docs: failed to start"
        })
    );
}

#[test]
fn serializes_model_call_started_event() {
    assert_eq!(
        serde_json::to_value(AgentEvent::ModelCallStarted).expect("serialize model call event"),
        json!({"type": "model_call_started"})
    );
}

#[test]
fn serializes_file_change_approval_and_tool_summary() {
    let file = FileChangeSummary {
        path: "src/lib.rs".to_string(),
        operation: FileChangeOperation::Update,
        replacements: 2,
        created: false,
        overwritten: true,
        deleted: false,
    };
    let request = ApprovalRequest::file_changes(
        "approval-call_1",
        vec![file.clone()],
        "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n",
        "file changes require approval",
    );
    let event = AgentEvent::ToolCallFinished {
        id: "call_1".to_string(),
        name: "apply_patch".to_string(),
        ok: true,
        summary: Some(ToolExecutionSummary::file_changes(
            vec![file],
            "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n",
        )),
    };

    let value = serde_json::to_value(json!({
        "request": request,
        "event": event,
    }))
    .expect("serialize file approval");

    assert_eq!(
        value,
        json!({
            "request": {
                "id": "approval-call_1",
                "action": {
                    "kind": "file_changes",
                    "files": [{
                        "path": "src/lib.rs",
                        "operation": "update",
                        "replacements": 2,
                        "created": false,
                        "overwritten": true,
                        "deleted": false
                    }],
                    "diff": "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n"
                },
                "reason": "file changes require approval"
            },
            "event": {
                "type": "tool_call_finished",
                "data": {
                    "id": "call_1",
                    "name": "apply_patch",
                    "ok": true,
                    "summary": {
                        "files": [{
                            "path": "src/lib.rs",
                            "operation": "update",
                            "replacements": 2,
                            "created": false,
                            "overwritten": true,
                            "deleted": false
                        }],
                        "diff": "--- src/lib.rs\n+++ src/lib.rs\n@@\n-old\n+new\n"
                    }
                }
            }
        })
    );
}

#[test]
fn omits_empty_tool_execution_summary() {
    let event = AgentEvent::ToolCallFinished {
        id: "call_1".to_string(),
        name: "read_file".to_string(),
        ok: true,
        summary: None,
    };

    let value = serde_json::to_value(&event).expect("serialize event");

    assert_eq!(
        value,
        json!({
            "type": "tool_call_finished",
            "data": {
                "id": "call_1",
                "name": "read_file",
                "ok": true
            }
        })
    );
}

#[test]
fn serializes_assistant_tool_call_and_tool_result_messages() {
    let tool_call = ToolCall::function("call_1", "read_file", r#"{"path":"Cargo.toml"}"#);
    let messages = vec![
        Message::assistant_tool_calls(vec![tool_call]),
        Message::tool_result("call_1", r#"{"ok":true}"#),
    ];

    let value = serde_json::to_value(&messages).expect("serialize messages");

    assert_eq!(
        value,
        json!([
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"Cargo.toml\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "content": "{\"ok\":true}",
                "tool_call_id": "call_1"
            }
        ])
    );
}

#[test]
fn serializes_assistant_tool_call_message_with_content() {
    let tool_call = ToolCall::function("call_1", "read_file", r#"{"path":"Cargo.toml"}"#);
    let message = Message::assistant_tool_calls_with_content("I will read it.", vec![tool_call]);

    let value = serde_json::to_value(&message).expect("serialize message");

    assert_eq!(
        value,
        json!({
            "role": "assistant",
            "content": "I will read it.",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": "{\"path\":\"Cargo.toml\"}"
                }
            }]
        })
    );
}

#[test]
fn turn_serializes_running_model_call_shape() {
    let turn = Turn::running(Message::user("Hello"));

    let value = serde_json::to_value(&turn).expect("serialize turn");

    assert_eq!(
        value,
        json!({
            "status": "running",
            "user_message": {"role": "user", "content": "Hello"},
            "assistant_message": null,
            "steps": [{
                "kind": "model_call",
                "status": "running",
                "error": null
            }],
            "error": null
        })
    );
}

#[test]
fn turn_records_completion_and_failure() {
    let mut completed = Turn::running(Message::user("Hello"));
    completed.complete(Message::assistant("Hi"));

    assert_eq!(completed.status, TurnStatus::Completed);
    assert_eq!(completed.assistant_message, Some(Message::assistant("Hi")));
    assert_eq!(completed.steps[0].status, TurnStatus::Completed);
    assert_eq!(completed.error, None);

    let mut failed = Turn::running(Message::user("Hello"));
    failed.fail("model error");

    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(failed.assistant_message, None);
    assert_eq!(failed.steps[0].status, TurnStatus::Failed);
    assert_eq!(failed.steps[0].error, Some("model error".to_string()));
    assert_eq!(failed.error, Some("model error".to_string()));
}

#[test]
fn failed_turn_closes_every_running_step() {
    let mut turn = Turn::running(Message::user("Hello"));
    turn.steps
        .push(TurnStep::running_tool_call("read_file", "call-1"));
    turn.steps
        .push(TurnStep::running_tool_call("list_files", "call-2"));

    turn.fail("turn cancelled");

    assert!(
        turn.steps
            .iter()
            .all(|step| step.status != TurnStatus::Running)
    );
    assert!(
        turn.steps
            .iter()
            .all(|step| step.error.as_deref() == Some("turn cancelled"))
    );
}

#[test]
fn turn_record_preserves_messages_for_completed_and_failed_turns() {
    let mut completed = Turn::running(Message::user("Hello"));
    completed.complete(Message::assistant("Hi"));
    let record = TurnRecord::new(
        completed.clone(),
        vec![Message::user("Hello"), Message::assistant("Hi")],
    );

    assert_eq!(record.turn, completed);
    assert_eq!(record.messages.len(), 2);

    let failed = TurnRecord::failed_user_prompt("Broken", "model error");

    assert_eq!(failed.turn.status, TurnStatus::Failed);
    assert_eq!(failed.messages, vec![Message::user("Broken")]);
    assert_eq!(failed.turn.error.as_deref(), Some("model error"));
}

#[test]
fn serializes_subagent_events_and_summary() {
    let summary = SubagentExecutionSummary::success(
        "Inspect session storage",
        "Sessions are scoped by workspace hash.",
        2,
        3,
        false,
    )
    .with_agent_identity(&SubagentIdentity {
        id: "builtin-01".to_string(),
        name: "后藤一里".to_string(),
    });
    let events = vec![
        AgentEvent::SubagentStarted {
            id: "call-1".to_string(),
            agent_id: summary.agent_id.clone(),
            agent_name: summary.agent_name.clone(),
            task: summary.task.clone(),
        },
        AgentEvent::SubagentFinished {
            id: "call-1".to_string(),
            ok: true,
            summary: summary.clone(),
        },
    ];

    assert_eq!(
        serde_json::to_value(events).expect("serialize subagent events"),
        json!([
            {
                "type": "subagent_started",
                "data": {
                    "id": "call-1",
                    "agent_id": "builtin-01",
                    "agent_name": "后藤一里",
                    "task": "Inspect session storage"
                }
            },
            {
                "type": "subagent_finished",
                "data": {
                    "id": "call-1",
                    "ok": true,
                    "summary": {
                        "agent_id": "builtin-01",
                        "agent_name": "后藤一里",
                        "task": "Inspect session storage",
                        "result": "Sessions are scoped by workspace hash.",
                        "model_calls": 2,
                        "tool_calls": 3,
                        "truncated": false
                    }
                }
            }
        ])
    );
    assert_eq!(
        serde_json::to_value(ToolExecutionSummary::subagent(summary))
            .expect("serialize subagent summary"),
        json!({
            "subagent": {
                "agent_id": "builtin-01",
                "agent_name": "后藤一里",
                "task": "Inspect session storage",
                "result": "Sessions are scoped by workspace hash.",
                "model_calls": 2,
                "tool_calls": 3,
                "truncated": false
            }
        })
    );

    let legacy_event: AgentEvent = serde_json::from_value(json!({
        "type": "subagent_started",
        "data": {
            "id": "legacy-call",
            "task": "Inspect legacy state"
        }
    }))
    .expect("deserialize legacy subagent event");
    assert!(matches!(
        legacy_event,
        AgentEvent::SubagentStarted {
            agent_id: None,
            agent_name: None,
            ..
        }
    ));
}

#[test]
fn v6_fact_lines_without_model_visible_fields_still_parse() {
    // v6 的 TurnStarted 没有 system_prompt，MiddlewareFinished 没有 injected_context。
    let turn_started: SessionFactEnvelope = serde_json::from_value(json!({
        "revision": 1,
        "timestamp_ms": 1,
        "operation_id": "operation-1",
        "turn_id": "turn-1",
        "fact": {
            "type": "turn_started",
            "data": {
                "user_message": {"role": "user", "content": "hello"},
                "model": {
                    "provider_id": "test",
                    "provider_name": "Test",
                    "model_id": "model",
                    "model_name": "Model",
                    "reasoning": "off"
                },
                "permissions": {"mode": "read_only", "shell": "deny"}
            }
        }
    }))
    .expect("parse v6 turn_started");
    assert!(matches!(
        turn_started.fact,
        SessionFact::TurnStarted {
            ref system_prompt,
            ..
        } if system_prompt.is_empty()
    ));

    let middleware_finished: SessionFactEnvelope = serde_json::from_value(json!({
        "revision": 2,
        "timestamp_ms": 2,
        "fact": {
            "type": "middleware_finished",
            "data": {
                "invocation": {
                    "invocation_id": "middleware-1",
                    "middleware_id": "policy",
                    "source": "internal",
                    "stage": "before_prompt",
                    "outcome": "continue",
                    "started_at_ms": 1,
                    "duration_ms": 2
                }
            }
        }
    }))
    .expect("parse v6 middleware_finished");
    assert!(matches!(
        middleware_finished.fact,
        SessionFact::MiddlewareFinished {
            ref invocation,
        } if invocation.injected_context.is_empty()
    ));
}

#[test]
fn model_visible_fact_fields_roundtrip() {
    let invocation = MiddlewareInvocationFinished {
        invocation_id: "middleware-1".to_string(),
        middleware_id: "policy".to_string(),
        source: MiddlewareSource::ProjectCommand,
        stage: MiddlewareStage::BeforePrompt,
        outcome: MiddlewareOutcome::Continue,
        started_at_ms: 1,
        duration_ms: 2,
        reason: None,
        injected_context: vec![MiddlewareContextBlock {
            middleware_id: "policy".to_string(),
            source: MiddlewareSource::ProjectCommand,
            stage: MiddlewareStage::BeforePrompt,
            content: "injected".to_string(),
        }],
    };
    let facts = vec![
        SessionFact::TurnStarted {
            user_message: Message::user("hello"),
            model: ModelInvocation {
                provider_id: "test".to_string(),
                provider_name: "Test".to_string(),
                model_id: "model".to_string(),
                model_name: "Model".to_string(),
                reasoning: ReasoningLevel::Off,
            },
            permissions: PermissionProfile::default(),
            system_prompt: "base\n\nguidance".to_string(),
        },
        SessionFact::MiddlewareFinished {
            invocation: invocation.clone(),
        },
        SessionFact::PromptRejected {
            prompt: "secret prompt".to_string(),
            reasons: vec!["policy: secret detected".to_string()],
        },
    ];

    for fact in facts {
        let bytes = serde_json::to_vec(&fact).expect("serialize fact");
        let parsed: SessionFact = serde_json::from_slice(&bytes).expect("parse fact");
        assert_eq!(parsed, fact);
    }
}
