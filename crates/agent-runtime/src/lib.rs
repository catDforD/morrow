pub mod session_store;

use agent_config::ContextConfig;
use agent_core::{Agent, AgentError};
use agent_model::{ModelError, ModelEvent, OpenAiCompatClient};
use agent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Conversation, Message, PermissionProfile,
    Session, TurnRecord, TurnStatus,
};
use agent_tools::{ToolRegistry, ToolRegistryError};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use agent_tools::LarkToolConfig;
pub use session_store::{SessionEntry, SessionStore, SessionStoreError};

pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Tools(#[from] ToolRegistryError),
    #[error(transparent)]
    SessionStore(#[from] SessionStoreError),
    #[error("agent run failed: {0}")]
    AgentRun(String),
    #[error("turn event handler failed: {0}")]
    EventHandler(String),
}

impl RuntimeError {
    pub fn event_handler(error: impl ToString) -> Self {
        Self::EventHandler(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    Changed,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    pub schema_version: u32,
    pub timestamp_ms: u64,
    pub session: String,
    pub workspace_root: String,
    pub turn_index: usize,
    pub event_index: usize,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, Copy)]
pub struct RunAgentTurnContext<'a> {
    pub client: &'a OpenAiCompatClient,
    pub system_prompt: &'a str,
    pub context_config: ContextConfig,
    pub workspace_root: &'a Path,
    pub permissions: PermissionProfile,
    pub lark: Option<&'a LarkToolConfig>,
    pub session_name: &'a str,
    pub turn_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentTurnOutcome {
    pub session_changed: bool,
    pub error: Option<String>,
}

pub trait TurnEventHandler {
    fn on_event(&mut self, _event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, RuntimeError>> {
        async move { Ok(ApprovalDecision::deny(request.id.clone())) }.boxed()
    }
}

pub async fn run_agent_turn(
    context: RunAgentTurnContext<'_>,
    session: &mut Session,
    prompt: &str,
    handler: &mut impl TurnEventHandler,
) -> Result<RunAgentTurnOutcome, RuntimeError> {
    if let Err(error) = maybe_auto_compact(
        context.client,
        context.system_prompt,
        session,
        context.context_config,
        prompt,
    )
    .await
    {
        let message = format!("context compaction failed: {error}");
        session
            .turns
            .push(TurnRecord::failed_user_prompt(prompt, message.clone()));
        return Ok(RunAgentTurnOutcome {
            session_changed: true,
            error: Some(message),
        });
    }

    let tools = match context.lark {
        Some(lark) => ToolRegistry::built_in_with_lark(
            context.workspace_root,
            context.permissions,
            lark.clone(),
        )?,
        None => ToolRegistry::built_in(context.workspace_root, context.permissions)?,
    };
    let agent = Agent::with_tools(
        context.client.clone(),
        context.system_prompt.to_string(),
        tools,
    );
    let mut agent_error = None;
    let mut turn_completed = false;
    let mut event_index = 0;

    {
        let mut stream = agent
            .run_turn(&mut session.active_thread, prompt.to_string())
            .await?;

        while let Some(event) = stream.next().await {
            let envelope = make_event_envelope(
                context.session_name,
                context.workspace_root,
                context.turn_index,
                event_index,
                event.clone(),
            );
            event_index += 1;
            handler.on_event(&envelope)?;

            match event {
                AgentEvent::ApprovalRequested(request) => {
                    let decision = handler.resolve_approval(&request).await?;
                    stream.resolve_approval(decision)?;
                }
                AgentEvent::TurnCompleted => {
                    turn_completed = true;
                }
                AgentEvent::Error(message) => {
                    agent_error = Some(message);
                }
                AgentEvent::TurnStarted
                | AgentEvent::TextDelta(_)
                | AgentEvent::AgentMessage(_)
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolCallFinished { .. }
                | AgentEvent::ApprovalResolved(_) => {}
            }
        }

        session.turns.push(stream.into_turn_record());
    }

    Ok(RunAgentTurnOutcome {
        session_changed: true,
        error: agent_error.filter(|_| !turn_completed),
    })
}

pub fn make_event_envelope(
    session_name: &str,
    workspace_root: &Path,
    turn_index: usize,
    event_index: usize,
    event: AgentEvent,
) -> AgentEventEnvelope {
    AgentEventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_ms: timestamp_ms(),
        session: session_name.to_string(),
        workspace_root: workspace_root.display().to_string(),
        turn_index,
        event_index,
        event,
    }
}

pub fn timestamp_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

pub async fn maybe_auto_compact(
    client: &OpenAiCompatClient,
    system_prompt: &str,
    session: &mut Session,
    context_config: ContextConfig,
    prompt: &str,
) -> Result<(), RuntimeError> {
    if !context_config.auto_compact {
        return Ok(());
    }

    let estimate = estimate_context_chars(system_prompt, session, prompt);
    if estimate <= context_config.max_context_chars {
        return Ok(());
    }

    compact_session(client, session, context_config).await?;

    let compacted_estimate = estimate_context_chars(system_prompt, session, prompt);
    if compacted_estimate > context_config.max_context_chars {
        return Err(RuntimeError::AgentRun(format!(
            "context is still over budget after compaction ({compacted_estimate} > {})",
            context_config.max_context_chars
        )));
    }

    Ok(())
}

pub async fn compact_session(
    client: &OpenAiCompatClient,
    session: &mut Session,
    context_config: ContextConfig,
) -> Result<CompactionOutcome, RuntimeError> {
    let prefix_len = compactable_prefix_len(session, context_config.retain_recent_turns);
    if prefix_len <= session.context.summarized_turns {
        return Ok(CompactionOutcome::Noop);
    }

    let records = session.turns[session.context.summarized_turns..prefix_len].to_vec();
    let summary = request_session_summary(
        client,
        session.context.summary.as_deref(),
        context_config.summary_target_chars,
        &records,
        session.context.summarized_turns,
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

pub fn detect_workspace_root() -> Result<PathBuf, RuntimeError> {
    let cwd = std::env::current_dir().map_err(SessionStoreError::CurrentDir)?;
    let mut candidate = cwd.as_path();

    loop {
        let manifest = candidate.join("Cargo.toml");
        if manifest.is_file() && manifest_has_workspace_header(&manifest) {
            return Ok(candidate.to_path_buf());
        }
        let Some(parent) = candidate.parent() else {
            return Ok(cwd);
        };
        candidate = parent;
    }
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
    client: &OpenAiCompatClient,
    existing_summary: Option<&str>,
    target_chars: usize,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> Result<String, RuntimeError> {
    let mut conversation = Conversation::with_system_prompt(
        "You compact long-running coding agent session history. Produce a concise, factual summary that preserves user goals, constraints, decisions, file and command results, failure reasons, pending tasks, and open questions. Do not include fluff.",
    );
    conversation.push(Message::user(build_summary_prompt(
        existing_summary,
        target_chars,
        records,
        first_turn_index,
    )));

    let mut stream = client.stream_chat(&conversation, &[]).await?;
    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ModelEvent::TextDelta(text) => summary.push_str(&text),
            ModelEvent::Completed => {
                let summary = summary.trim().to_string();
                if summary.is_empty() {
                    return Err(RuntimeError::AgentRun(
                        "summary model returned an empty summary".to_string(),
                    ));
                }
                return Ok(summary);
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

fn build_summary_prompt(
    existing_summary: Option<&str>,
    target_chars: usize,
    records: &[TurnRecord],
    first_turn_index: usize,
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(
        prompt,
        "Update the session summary. Target length: at most {target_chars} characters."
    );
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

fn estimate_context_chars(system_prompt: &str, session: &Session, prompt: &str) -> usize {
    system_prompt.chars().count()
        + prompt.chars().count()
        + session
            .active_thread
            .messages
            .iter()
            .map(message_context_chars)
            .sum::<usize>()
}

fn message_context_chars(message: &Message) -> usize {
    let mut total = message_role_label(message).len();
    if let Some(content) = message.content.as_ref() {
        total += content.chars().count();
    }
    if let Some(tool_call_id) = message.tool_call_id.as_ref() {
        total += tool_call_id.chars().count();
    }
    if let Some(tool_calls) = message.tool_calls.as_ref() {
        total += serde_json::to_string(tool_calls)
            .map(|value| value.chars().count())
            .unwrap_or_default();
    }
    total
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

fn manifest_has_workspace_header(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| line.trim() == "[workspace]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::OpenAiCompatConfig;
    use agent_protocol::{FileChangeOperation, SessionContext, Thread, Turn};
    use futures_util::future::BoxFuture;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-runtime-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create root");
        path
    }

    fn context_config(max_context_chars: usize, retain_recent_turns: usize) -> ContextConfig {
        ContextConfig {
            auto_compact: true,
            max_context_chars,
            retain_recent_turns,
            summary_target_chars: 256,
        }
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
            context: SessionContext::new(),
        };
        rebuild_active_thread(&mut session);
        session
    }

    #[derive(Default)]
    struct RecordingHandler {
        events: Vec<AgentEventEnvelope>,
    }

    impl TurnEventHandler for RecordingHandler {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            self.events.push(event.clone());
            Ok(())
        }
    }

    struct ApprovalHandler {
        events: Vec<AgentEventEnvelope>,
        approved: bool,
    }

    impl TurnEventHandler for ApprovalHandler {
        fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
            self.events.push(event.clone());
            Ok(())
        }

        fn resolve_approval<'a>(
            &'a mut self,
            request: &'a ApprovalRequest,
        ) -> BoxFuture<'a, Result<ApprovalDecision, RuntimeError>> {
            async move {
                Ok(if self.approved {
                    ApprovalDecision::approve(request.id.clone())
                } else {
                    ApprovalDecision::deny(request.id.clone())
                })
            }
            .boxed()
        }
    }

    #[test]
    fn event_envelope_uses_stable_schema_and_indices() {
        let root = unique_dir("envelope");
        let envelope = make_event_envelope("default", &root, 7, 3, AgentEvent::TurnStarted);

        assert_eq!(envelope.schema_version, EVENT_SCHEMA_VERSION);
        assert!(envelope.timestamp_ms > 0);
        assert_eq!(envelope.session, "default");
        assert_eq!(envelope.workspace_root, root.display().to_string());
        assert_eq!(envelope.turn_index, 7);
        assert_eq!(envelope.event_index, 3);
        assert_eq!(envelope.event, AgentEvent::TurnStarted);
    }

    #[tokio::test]
    async fn manual_compaction_summarizes_old_turns_and_rebuilds_active_context() {
        let (base_url, requests) =
            spawn_recording_sse_server(vec![sse_text_body("new summary")]).await;
        let mut session = compactable_session();

        let outcome = compact_session(&client(base_url), &mut session, context_config(10_000, 2))
            .await
            .expect("compact session");

        assert_eq!(outcome, CompactionOutcome::Changed);
        assert_eq!(session.context.summary.as_deref(), Some("new summary"));
        assert_eq!(session.context.summarized_turns, 3);
        assert_eq!(
            session.active_thread.messages,
            vec![
                Message::system("Session summary:\nnew summary"),
                Message::user("u3"),
                Message::assistant("a3"),
                Message::user("u4"),
                Message::assistant("a4"),
            ]
        );

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("failure reason"));
        assert!(requests[0].contains("Target length: at most 256 characters"));
    }

    #[tokio::test]
    async fn run_agent_turn_records_completed_turn_and_event_envelopes() {
        let root = unique_dir("run-success");
        let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = RecordingHandler::default();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                system_prompt: "system",
                context_config: context_config(10_000, 2),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                lark: None,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "hello",
            &mut handler,
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
        assert_eq!(session.turns[0].messages, session.active_thread.messages);
        assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
        assert_eq!(
            handler
                .events
                .iter()
                .map(|event| event.event_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            handler.events[1].event,
            AgentEvent::TextDelta("ok".to_string())
        );
    }

    #[tokio::test]
    async fn approval_deny_path_resumes_stream_and_records_turn() {
        let root = unique_dir("approval-deny");
        let first_body = tool_call_body(
            "call_1",
            "write_file",
            json!({
                "path": "note.txt",
                "content": "created\n"
            }),
        );
        let second_body = sse_text_body("Denied");
        let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let client = client(base_url);
        let mut session = Session::new();
        let mut handler = ApprovalHandler {
            events: Vec::new(),
            approved: false,
        };

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                system_prompt: "system",
                context_config: context_config(10_000, 2),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(
                    agent_protocol::PermissionMode::WorkspaceWrite,
                ),
                lark: None,
                session_name: "default",
                turn_index: 0,
            },
            &mut session,
            "write note",
            &mut handler,
        )
        .await
        .expect("run turn");

        assert_eq!(outcome.error, None);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
        assert!(
            handler
                .events
                .iter()
                .any(|event| matches!(event.event, AgentEvent::ApprovalRequested(_)))
        );
        assert!(handler.events.iter().any(|event| {
            matches!(
                &event.event,
                AgentEvent::ApprovalResolved(decision) if !decision.approved
            )
        }));
    }

    #[tokio::test]
    async fn auto_compaction_failure_records_failed_turn_without_main_model_call() {
        let root = unique_dir("run-compact-fail");
        let (base_url, requests) = spawn_recording_sse_server(vec!["data: {not-json}\n\n"]).await;
        let client = client(base_url);
        let mut session = compactable_session();
        session.active_thread.push(Message::user(
            "large active context that exceeds the tiny budget",
        ));
        let original_active_thread = session.active_thread.clone();
        let mut handler = RecordingHandler::default();

        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                system_prompt: "system",
                context_config: context_config(1, 2),
                workspace_root: &root,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                lark: None,
                session_name: "default",
                turn_index: session.turns.len(),
            },
            &mut session,
            "hello",
            &mut handler,
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
        assert_eq!(session.active_thread, original_active_thread);
        assert_eq!(session.turns.len(), 6);
        assert_eq!(
            session.turns.last().expect("failed turn").turn.status,
            TurnStatus::Failed
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
        assert!(handler.events.is_empty());
    }

    #[test]
    fn file_summary_helper_is_available_to_tests() {
        let file = agent_protocol::FileChangeSummary {
            path: "note.txt".to_string(),
            operation: FileChangeOperation::Add,
            replacements: 0,
            created: true,
            overwritten: false,
            deleted: false,
        };

        assert_eq!(file.operation.as_str(), "add");
    }
}
