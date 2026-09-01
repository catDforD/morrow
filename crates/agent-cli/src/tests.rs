use super::*;
use agent_protocol::{FileChangeOperation, Message, Thread, Turn, TurnRecord, TurnStatus};
use agent_runtime::{compact_session, rebuild_active_thread};
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn test_model_invocation() -> &'static ModelInvocation {
    static MODEL: OnceLock<ModelInvocation> = OnceLock::new();
    MODEL.get_or_init(|| config_model_invocation("test-model"))
}

async fn spawn_recording_sse_server(
    bodies: Vec<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    tokio::spawn(async move {
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 8192];
            let read = socket.read(&mut request).await.expect("read request");
            captured_requests
                .lock()
                .expect("requests lock poisoned")
                .push(String::from_utf8_lossy(&request[..read]).to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });
    (format!("http://{addr}/v1"), requests)
}

fn client(base_url: String) -> OpenAiCompatClient {
    OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: 1,
    })
    .expect("client")
}

fn sse_text_body(text: &str) -> &'static str {
    let body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "delta": {"content": text},
                "finish_reason": null
            }]
        })
    );
    Box::leak(body.into_boxed_str())
}

fn tool_call_body(id: &str, name: &str, arguments: serde_json::Value) -> &'static str {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        })
    );
    Box::leak(body.into_boxed_str())
}

fn unique_cli_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("morrow-cli-{name}-{stamp}"));
    fs::create_dir_all(&path).expect("create root");
    path
}

fn context_config(retain_recent_turns: usize) -> ContextConfig {
    ContextConfig {
        auto_compact: true,
        auto_compact_threshold: 0.835,
        retain_recent_turns,
        summary_target_tokens: 256,
        compact_max_retries: 2,
        max_context_tokens: Some(300_000),
    }
}

fn model_limits(context_window_tokens: usize) -> ModelContextLimits {
    ModelContextLimits {
        context_window_tokens,
        reserved_output_tokens: 128,
    }
}

fn valid_compact_summary_text(current_progress: &str) -> String {
    format!(
        r#"User Goals and Constraints
- keep user intent

Important Decisions
- compact

Files and Code State
- none

Commands, Results, and Errors
- none

Current Progress
- {current_progress}

Pending Tasks
- none

Open Questions
- none"#
    )
}

fn valid_compact_summary(current_progress: &str) -> String {
    format!(
        r#"<analysis>
compact test
</analysis>
<summary>
{}
</summary>"#,
        valid_compact_summary_text(current_progress)
    )
}

fn completed_record(user: &str, assistant: &str) -> TurnRecord {
    let user_message = Message::user(user);
    let assistant_message = Message::assistant(assistant);
    let mut turn = Turn::running(user_message.clone());
    turn.complete(assistant_message.clone());
    TurnRecord::new(turn, vec![user_message, assistant_message])
}

fn compactable_session() -> Session {
    let turns = vec![
        completed_record("u0", "a0"),
        completed_record("u1", "a1"),
        TurnRecord::failed_user_prompt("broken", "failure reason"),
        completed_record("u3", "a3"),
        completed_record("u4", "a4"),
    ];
    let mut session = Session {
        active_thread: Thread::new(),
        turns,
        context: agent_protocol::SessionContext::new(),
    };
    rebuild_active_thread(&mut session);
    session
}

fn file_summary() -> FileChangeSummary {
    FileChangeSummary {
        path: "note.txt".to_string(),
        operation: FileChangeOperation::Add,
        replacements: 0,
        created: true,
        overwritten: false,
        deleted: false,
    }
}

#[test]
fn read_input_line_accepts_valid_utf8() {
    let mut input = std::io::Cursor::new(vec![0xe4, 0xbd, 0xa0, b'\n']);

    let line = read_input_line(&mut input)
        .expect("read line")
        .expect("line");

    assert_eq!(line.text, "\u{4f60}\n");
    assert!(!line.had_invalid_utf8);
}

#[test]
fn read_input_line_replaces_invalid_utf8() {
    let mut input = std::io::Cursor::new(vec![b'h', 0xff, b'i', b'\n']);

    let line = read_input_line(&mut input)
        .expect("read line")
        .expect("line");

    assert_eq!(line.text, "h\u{fffd}i\n");
    assert!(line.had_invalid_utf8);
}

#[test]
fn read_input_line_returns_none_on_eof() {
    let mut input = std::io::Cursor::new(Vec::new());

    let line = read_input_line(&mut input).expect("read eof");

    assert_eq!(line, None);
}

#[test]
fn parses_permission_modes_for_cli_and_repl() {
    assert_eq!(
        parse_permission_mode("read-only").expect("read-only"),
        PermissionMode::ReadOnly
    );
    assert_eq!(
        parse_permission_mode("workspace_write").expect("workspace_write"),
        PermissionMode::WorkspaceWrite
    );
    assert_eq!(
        parse_permission_mode("danger-full-access").expect("danger-full-access"),
        PermissionMode::DangerFullAccess
    );
    assert!(parse_permission_mode("full").is_err());
}

#[test]
fn resolves_session_cli_args_and_thread_alias() {
    let default_args = Args::try_parse_from(["morrow"]).expect("default args");
    assert_eq!(
        resolve_session_name(&default_args).expect("session"),
        "default"
    );

    let session_args = Args::try_parse_from(["morrow", "--session", "work"]).expect("session args");
    assert_eq!(
        resolve_session_name(&session_args).expect("session"),
        "work"
    );

    let thread_args = Args::try_parse_from(["morrow", "--thread", "legacy"]).expect("thread args");
    assert_eq!(
        resolve_session_name(&thread_args).expect("session"),
        "legacy"
    );

    let conflicting = Args::try_parse_from(["morrow", "--session", "work", "--thread", "legacy"])
        .expect("parse conflicting aliases");
    assert!(matches!(
        resolve_session_name(&conflicting),
        Err(CliError::ConflictingSessionArgs)
    ));
}

#[test]
fn parses_server_flags() {
    let default_args = Args::try_parse_from(["morrow", "server"]).expect("parse server");
    assert!(matches!(
        default_args.command,
        Some(CliCommand::Server {
            no_auth: false,
            permission_ceiling: None,
            ..
        })
    ));

    let args = Args::try_parse_from([
        "morrow",
        "server",
        "--port",
        "3100",
        "--no-auth",
        "--permission-ceiling",
        "workspace-write",
    ])
    .expect("parse server flags");
    assert!(matches!(
        args.command,
        Some(CliCommand::Server {
            port: 3100,
            no_auth: true,
            permission_ceiling: Some(PermissionMode::WorkspaceWrite),
            ..
        })
    ));
}

#[test]
fn parses_session_subcommands() {
    let init_args =
        Args::try_parse_from(["morrow", "init", "--template"]).expect("parse init template");
    assert!(matches!(
        init_args.command,
        Some(CliCommand::Init {
            force: false,
            template: true
        })
    ));

    let force_init_args =
        Args::try_parse_from(["morrow", "init", "--force"]).expect("parse init force");
    assert!(matches!(
        force_init_args.command,
        Some(CliCommand::Init {
            force: true,
            template: false
        })
    ));

    let list_args =
        Args::try_parse_from(["morrow", "session", "list"]).expect("parse session list");
    assert!(matches!(
        list_args.command,
        Some(CliCommand::Session {
            command: SessionCommand::List
        })
    ));

    let export_args = Args::try_parse_from([
        "morrow",
        "--session",
        "work",
        "session",
        "export",
        "--output",
        "session.json",
    ])
    .expect("parse session export");
    assert!(matches!(
        export_args.command,
        Some(CliCommand::Session {
            command: SessionCommand::Export { .. }
        })
    ));
    assert_eq!(resolve_session_name(&export_args).expect("session"), "work");
}

#[test]
fn parses_hooks_subcommands() {
    let list =
        Args::try_parse_from(["morrow", "hooks", "list", "--json"]).expect("parse hooks list");
    assert!(matches!(
        list.command,
        Some(CliCommand::Hooks {
            command: HooksCommand::List { json: true }
        })
    ));
    let trust = Args::try_parse_from(["morrow", "hooks", "trust"]).expect("parse hooks trust");
    assert!(matches!(
        trust.command,
        Some(CliCommand::Hooks {
            command: HooksCommand::Trust
        })
    ));
    let revoke = Args::try_parse_from(["morrow", "hooks", "revoke"]).expect("parse hooks revoke");
    assert!(matches!(
        revoke.command,
        Some(CliCommand::Hooks {
            command: HooksCommand::Revoke
        })
    ));
}

#[test]
fn hooks_commands_list_trust_and_revoke_project_configuration() {
    let home = unique_cli_dir("hooks-home");
    let workspace = unique_cli_dir("hooks-workspace");
    let manager = HookManager::new(&home, &workspace);
    fs::create_dir_all(
        manager
            .project_config_path()
            .parent()
            .expect("project hook parent"),
    )
    .expect("create project hook parent");
    fs::write(
            manager.project_config_path(),
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
        )
        .expect("write project hooks");

    let mut json_output = Vec::new();
    handle_hooks_command_with_manager(
        &HooksCommand::List { json: true },
        &manager,
        &mut json_output,
    )
    .expect("list hooks");
    let listed: serde_json::Value = serde_json::from_slice(&json_output).expect("hooks JSON");
    assert_eq!(listed["project_trusted"], false);
    assert_eq!(listed["hooks"][0]["active"], false);

    let mut trust_output = Vec::new();
    handle_hooks_command_with_manager(&HooksCommand::Trust, &manager, &mut trust_output)
        .expect("trust hooks");
    let trust_output = String::from_utf8(trust_output).expect("trust output");
    assert!(trust_output.contains("project hooks: trusted"));
    assert!(trust_output.contains("project\tbefore_prompt\tProject\tactive"));

    let mut revoke_output = Vec::new();
    handle_hooks_command_with_manager(&HooksCommand::Revoke, &manager, &mut revoke_output)
        .expect("revoke hooks");
    assert!(
        String::from_utf8(revoke_output)
            .expect("revoke output")
            .contains("project hooks: not trusted")
    );
}

#[test]
fn init_config_writes_global_config_template() {
    let home = unique_cli_dir("init-home");
    let path = default_config_path_for_home(&home);

    write_init_config(&path, INIT_CONFIG_API_KEY_PLACEHOLDER, false).expect("write init config");

    let content = fs::read_to_string(path).expect("read init config");
    assert!(content.contains(r#"base_url = "https://api.openai.com/v1""#));
    assert!(content.contains(r#"model = "gpt-4.1""#));
    assert!(content.contains(r#"OPENAI_API_KEY = "replace-with-your-openai-api-key""#));
    assert!(content.contains(r#"mode = "read_only""#));
    assert!(content.contains(r#"shell = "deny""#));
}

#[test]
fn init_config_refuses_existing_file_unless_forced() {
    let home = unique_cli_dir("init-force-home");
    let path = default_config_path_for_home(&home);
    write_init_config(&path, "first-key", false).expect("write first config");

    let err = write_init_config(&path, "second-key", false).expect_err("must not overwrite");

    assert!(matches!(err, CliError::ConfigExists { .. }));
    assert!(
        fs::read_to_string(&path)
            .expect("read preserved config")
            .contains("first-key")
    );

    write_init_config(&path, "second-key", true).expect("force overwrite");
    assert!(
        fs::read_to_string(path)
            .expect("read overwritten config")
            .contains("second-key")
    );
}

#[test]
fn jsonl_requires_prompt() {
    let args = Args::try_parse_from(["morrow", "--jsonl"]).expect("parse jsonl");

    assert!(matches!(
        validate_jsonl_prompt(&args, ""),
        Err(CliError::JsonlRequiresPrompt)
    ));
    assert!(validate_jsonl_prompt(&args, "hello").is_ok());
}

#[test]
fn effective_permissions_apply_cli_overrides() {
    let base = PermissionProfile {
        mode: PermissionMode::WorkspaceWrite,
        shell: ShellPolicy::Deny,
    };

    assert_eq!(effective_permissions(base, None, false), base);
    assert_eq!(
        effective_permissions(base, Some(PermissionMode::DangerFullAccess), false),
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess)
    );
    assert_eq!(
        effective_permissions(base, None, true),
        PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Allow,
        }
    );
}

#[test]
fn formats_file_change_approval_request_with_diff() {
    let request = ApprovalRequest::file_changes(
        "approval-call_1",
        vec![file_summary()],
        "--- /dev/null\n+++ note.txt\n@@\n+created\n",
        "file changes require approval",
    );

    let text = format_approval_request(
        &request,
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
    );

    assert!(text.contains("approval required: file changes require approval"));
    assert!(text.contains("action: file changes"));
    assert!(text.contains("- note.txt (add"));
    assert!(text.contains("+++ note.txt"));
    assert!(text.contains("permissions: mode=workspace_write, shell=prompt"));
}

#[test]
fn formats_mcp_tool_approval_request() {
    let request = ApprovalRequest::mcp_tool(
        "approval-call_1",
        "docs",
        "write_page",
        r#"{"path":"/index","content":"hello"}"#,
        "MCP tool 'write_page' on server 'docs' requires approval",
    );

    let text = format_approval_request(
        &request,
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
    );

    assert!(text.contains("action: mcp tool"));
    assert!(text.contains("server: docs"));
    assert!(text.contains("tool: write_page"));
    assert!(text.contains(r#"arguments: {"path":"/index","content":"hello"}"#));
    assert!(text.contains("permissions: mode=workspace_write, shell=prompt"));
}

#[test]
fn formats_execution_summary_for_file_shell_subagent_and_error_results() {
    let records = vec![
        ExecutionRecord {
            name: "write_file".to_string(),
            ok: true,
            summary: Some(ToolExecutionSummary::file_changes(
                vec![file_summary()],
                "--- /dev/null\n+++ note.txt\n@@\n+created\n",
            )),
        },
        ExecutionRecord {
            name: "shell_command".to_string(),
            ok: true,
            summary: Some(ToolExecutionSummary::shell(ShellCommandSummary {
                command: "cargo test".to_string(),
                exit_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
        },
        ExecutionRecord {
            name: "delegate_task".to_string(),
            ok: true,
            summary: Some(ToolExecutionSummary::subagent(
                agent_protocol::SubagentExecutionSummary::success(
                    "Inspect runtime",
                    "Runtime is ready.",
                    2,
                    1,
                    false,
                )
                .with_agent_name("后藤一里"),
            )),
        },
        ExecutionRecord {
            name: "edit_file".to_string(),
            ok: false,
            summary: Some(ToolExecutionSummary::error("approval denied")),
        },
    ];

    let text = format_execution_summary(&records).expect("summary");

    assert!(text.contains("execution summary:"));
    assert!(text.contains("- write_file: ok"));
    assert!(text.contains("diff: available"));
    assert!(text.contains("shell: exit_code=0"));
    assert!(text.contains("agent: 后藤一里"));
    assert!(text.contains("task: Inspect runtime"));
    assert!(text.contains("- edit_file: error"));
    assert!(text.contains("error: approval denied"));
}

#[tokio::test]
async fn manual_compaction_summarizes_old_turns_and_rebuilds_active_context() {
    let summary = valid_compact_summary("new summary");
    let summary_text = valid_compact_summary_text("new summary");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body(&summary)]).await;
    let mut session = compactable_session();

    let outcome = compact_session(&client(base_url), &mut session, context_config(2))
        .await
        .expect("compact session");

    assert_eq!(outcome, CompactionOutcome::Changed);
    assert_eq!(
        session.context.summary.as_deref(),
        Some(summary_text.as_str())
    );
    assert_eq!(session.context.summarized_turns, 3);
    assert_eq!(
        session.active_thread.messages,
        vec![
            Message::system(format!("Session summary:\n{summary_text}")),
            Message::user("u3"),
            Message::assistant("a3"),
            Message::user("u4"),
            Message::assistant("a4"),
        ]
    );

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("failure reason"));
    assert!(requests[0].contains("Target length: at most 256 tokens"));
}

#[tokio::test]
async fn run_agent_turn_records_completed_turn_in_history_and_active_context() {
    let root = unique_cli_dir("run-success");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut output = Vec::new();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            workspace_instructions: None,
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: &ToolsConfig::default(),
            auto_approve_workspace_writes: true,
            interactive_approvals: false,
            output: OutputMode::Human,
        },
        &mut session,
        "hello",
        &mut output,
    )
    .await
    .expect("run turn");

    assert_eq!(
        outcome,
        RunAgentTurnOutcome {
            session_changed: true,
            error: None,
        }
    );
    assert_eq!(
        session.active_thread.messages,
        vec![Message::user("hello"), Message::assistant("ok")]
    );
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
    assert_eq!(
        session.turns[0].turn.model.as_ref(),
        Some(test_model_invocation())
    );
    assert_eq!(session.turns[0].messages, session.active_thread.messages);
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
}

#[tokio::test]
async fn run_agent_turn_jsonl_outputs_event_envelopes() {
    let root = unique_cli_dir("jsonl-text");
    let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut output = Vec::new();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            workspace_instructions: None,
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: &ToolsConfig::default(),
            auto_approve_workspace_writes: true,
            interactive_approvals: false,
            output: OutputMode::Jsonl {
                session_name: "default",
                turn_index: 0,
            },
        },
        &mut session,
        "hello",
        &mut output,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    let text = String::from_utf8(output).expect("utf8 output");
    let lines = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[0]["schema_version"], json!(8));
    assert!(lines[0]["timestamp_ms"].as_u64().is_some());
    assert_eq!(lines[0]["session"], "default");
    assert_eq!(lines[0]["workspace_root"], root.display().to_string());
    assert_eq!(lines[0]["turn_index"], json!(0));
    assert_eq!(lines[0]["event_index"], json!(0));
    assert_eq!(lines[0]["event"], json!({"type": "turn_started"}));
    assert_eq!(lines[1]["event"], json!({"type": "model_call_started"}));
    assert_eq!(
        lines[2]["event"],
        json!({"type": "text_delta", "data": "ok"})
    );
    assert_eq!(
        lines[3]["event"],
        json!({
            "type": "model_message_committed",
            "data": {
                "model_call_id": "model-call-0",
                "message": {"role": "assistant", "content": "ok"}
            }
        })
    );
    assert_eq!(
        lines[4]["event"],
        json!({"type": "agent_message", "data": "ok"})
    );
    assert_eq!(lines[5]["event"], json!({"type": "turn_completed"}));
}

#[test]
fn jsonl_subagent_events_include_the_assigned_name() {
    let root = unique_cli_dir("jsonl-subagent-name");
    let envelope = agent_runtime::make_event_envelope(
        "default",
        &root,
        2,
        3,
        AgentEvent::SubagentStarted {
            id: "call-1".to_string(),
            agent_id: Some("builtin-01".to_string()),
            agent_name: Some("后藤一里".to_string()),
            task: "Inspect runtime".to_string(),
        },
    );
    let mut output = Vec::new();

    write_jsonl_event(&mut output, &envelope).expect("write JSONL event");

    let value: serde_json::Value = serde_json::from_slice(&output).expect("parse JSONL event");
    assert_eq!(value["schema_version"], json!(8));
    assert_eq!(
        value["event"],
        json!({
            "type": "subagent_started",
            "data": {
                "id": "call-1",
                "agent_id": "builtin-01",
                "agent_name": "后藤一里",
                "task": "Inspect runtime"
            }
        })
    );
}

#[tokio::test]
async fn run_agent_turn_jsonl_outputs_mcp_warning_events() {
    let root = unique_cli_dir("jsonl-mcp-warning");
    let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut output = Vec::new();
    let mcp_cache = McpToolCache::new();
    let mcp_servers = vec![McpServerConfig {
        name: "bad".to_string(),
        transport: agent_config::McpTransport::Stdio,
        command: "definitely-not-a-real-morrow-mcp-command".to_string(),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: None,
        http_headers: Default::default(),
        enabled: true,
        startup_timeout_sec: 1,
        tool_timeout_sec: 1,
        require_approval: None,
    }];

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            workspace_instructions: None,
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &mcp_servers,
            mcp_cache: &mcp_cache,
            tools: &ToolsConfig::default(),
            auto_approve_workspace_writes: true,
            interactive_approvals: false,
            output: OutputMode::Jsonl {
                session_name: "default",
                turn_index: 0,
            },
        },
        &mut session,
        "hello",
        &mut output,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    let text = String::from_utf8(output).expect("utf8 output");
    let lines = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert_eq!(lines[0]["event"]["type"], "warning");
    assert!(
        lines[0]["event"]["data"]
            .as_str()
            .expect("warning text")
            .contains("mcp server bad")
    );
    assert_eq!(lines[1]["event"], json!({"type": "turn_started"}));
    assert_eq!(
        lines.last().expect("last")["event"]["type"],
        "turn_completed"
    );
}

#[tokio::test]
async fn run_agent_turn_jsonl_suppresses_human_execution_summary() {
    let root = unique_cli_dir("jsonl-tool");
    fs::write(root.join("note.txt"), "tool result\n").expect("write note");
    let first_body = tool_call_body(
        "call_1",
        "read_file",
        json!({"path": "note.txt", "max_lines": 5}),
    );
    let second_body = sse_text_body("done");
    let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut output = Vec::new();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            workspace_instructions: None,
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: &ToolsConfig::default(),
            auto_approve_workspace_writes: true,
            interactive_approvals: false,
            output: OutputMode::Jsonl {
                session_name: "default",
                turn_index: 0,
            },
        },
        &mut session,
        "read note",
        &mut output,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    let text = String::from_utf8(output).expect("utf8 output");
    assert!(!text.contains("execution summary:"));
    let lines = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line["event"]["type"] == "tool_call_finished")
    );
}

#[tokio::test]
async fn auto_compaction_failure_records_failed_turn_without_main_model_call() {
    let root = unique_cli_dir("run-compact-fail");
    let (base_url, requests) = spawn_recording_sse_server(vec!["data: {not-json}\n\n"]).await;
    let client = client(base_url);
    let mut session = compactable_session();
    session.active_thread.push(Message::user(
        "large active context that exceeds the tiny budget",
    ));
    let mut output = Vec::new();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            workspace_instructions: None,
            context_config: context_config(2),
            model_limits: model_limits(1),
            workspace_root: &root,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: &ToolsConfig::default(),
            auto_approve_workspace_writes: true,
            interactive_approvals: false,
            output: OutputMode::Human,
        },
        &mut session,
        "hello",
        &mut output,
    )
    .await
    .expect("run turn");

    assert!(matches!(
        outcome,
        RunAgentTurnOutcome {
            session_changed: true,
            error: Some(_),
        }
    ));
    assert!(session.context.summary.is_some());
    assert_ne!(session.context.summarized_turns, 0);
    assert_eq!(session.turns.len(), 6);
    assert_eq!(
        session.turns.last().expect("failed turn").turn.status,
        TurnStatus::Failed
    );
    assert_eq!(
        session
            .turns
            .last()
            .expect("failed turn")
            .turn
            .model
            .as_ref(),
        Some(test_model_invocation())
    );
    assert!(
        session
            .turns
            .last()
            .expect("failed turn")
            .turn
            .error
            .as_deref()
            .expect("error")
            .contains("context compaction failed")
    );
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
}
