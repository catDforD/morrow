use agent_model::{ChatCompletionStream, ModelError, ModelEvent, OpenAiCompatClient};
use agent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Conversation, Message, Thread, ToolCall, Turn,
    TurnStep,
};
use agent_tools::{ToolExecution, ToolRegistry, ToolResult};
use futures_util::Stream;
use futures_util::future::{BoxFuture, FutureExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

#[derive(Debug, Clone)]
pub struct Agent {
    client: OpenAiCompatClient,
    system_prompt: String,
    tools: ToolRegistry,
    max_tool_rounds: usize,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("{0}")]
    Model(#[from] ModelError),
    #[error("{0}")]
    Approval(String),
}

impl Agent {
    pub fn new(client: OpenAiCompatClient, system_prompt: impl Into<String>) -> Self {
        Self {
            client,
            system_prompt: system_prompt.into(),
            tools: ToolRegistry::empty(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }

    pub fn with_tools(
        client: OpenAiCompatClient,
        system_prompt: impl Into<String>,
        tools: ToolRegistry,
    ) -> Self {
        Self {
            client,
            system_prompt: system_prompt.into(),
            tools,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }

    pub async fn run_turn<'a>(
        &self,
        thread: &'a mut Thread,
        prompt: impl Into<String>,
    ) -> Result<AgentTurnStream<'a>, AgentError> {
        let user_message = Message::user(prompt.into());
        let mut conversation = Conversation::with_system_prompt(self.system_prompt.clone());
        conversation.messages.extend(thread.messages.clone());
        conversation.push(user_message.clone());

        let model_stream = self
            .client
            .stream_chat(&conversation, self.tools.definitions())
            .await?;

        Ok(AgentTurnStream {
            client: self.client.clone(),
            tools: self.tools.clone(),
            max_tool_rounds: self.max_tool_rounds,
            conversation,
            model_stream: Some(model_stream),
            model_start: None,
            pending_tool_calls: VecDeque::new(),
            tool_future: None,
            pending_approval: None,
            thread,
            turn: Turn::running(user_message.clone()),
            turn_messages: vec![user_message.clone()],
            assistant_text: String::new(),
            assistant_deltas: VecDeque::new(),
            pending: VecDeque::from([AgentEvent::TurnStarted]),
            finished: false,
            tool_rounds: 0,
        })
    }
}

type ModelStartFuture = BoxFuture<'static, Result<ChatCompletionStream, ModelError>>;
type ToolCallFuture = BoxFuture<'static, (ToolCall, ToolExecution)>;

#[derive(Debug, Clone)]
struct PendingApproval {
    tool_call: ToolCall,
    request: ApprovalRequest,
}

pub struct AgentTurnStream<'a> {
    client: OpenAiCompatClient,
    tools: ToolRegistry,
    max_tool_rounds: usize,
    conversation: Conversation,
    model_stream: Option<ChatCompletionStream>,
    model_start: Option<ModelStartFuture>,
    pending_tool_calls: VecDeque<ToolCall>,
    tool_future: Option<ToolCallFuture>,
    pending_approval: Option<PendingApproval>,
    thread: &'a mut Thread,
    turn: Turn,
    turn_messages: Vec<Message>,
    assistant_text: String,
    assistant_deltas: VecDeque<String>,
    pending: VecDeque<AgentEvent>,
    finished: bool,
    tool_rounds: usize,
}

impl AgentTurnStream<'_> {
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn into_turn(self) -> Turn {
        self.turn
    }

    pub fn resolve_approval(&mut self, decision: ApprovalDecision) -> Result<(), AgentError> {
        let Some(pending_approval) = self.pending_approval.take() else {
            return Err(AgentError::Approval(
                "received approval decision but no approval is pending".to_string(),
            ));
        };

        if decision.request_id != pending_approval.request.id {
            let expected = pending_approval.request.id.clone();
            self.pending_approval = Some(pending_approval);
            return Err(AgentError::Approval(format!(
                "approval decision {} does not match pending approval {expected}",
                decision.request_id
            )));
        }

        self.pending
            .push_back(AgentEvent::ApprovalResolved(decision.clone()));

        if decision.approved {
            self.start_approved_tool_call(
                pending_approval.tool_call,
                decision,
                pending_approval.request,
            );
        } else {
            let result = ToolResult::error("approval denied");
            self.finish_tool_call(pending_approval.tool_call, result);
        }

        Ok(())
    }

    fn complete_turn(&mut self) {
        let assistant_text = self.assistant_text.clone();
        let assistant_message = Message::assistant(assistant_text.clone());
        self.turn_messages.push(assistant_message.clone());
        self.thread.messages.extend(self.turn_messages.clone());
        self.turn.complete(assistant_message);
        for text in self.assistant_deltas.drain(..) {
            self.pending.push_back(AgentEvent::TextDelta(text));
        }
        self.pending
            .push_back(AgentEvent::AgentMessage(assistant_text));
        self.pending.push_back(AgentEvent::TurnCompleted);
        self.finished = true;
    }

    fn fail_turn(&mut self, error: impl ToString) {
        let error = error.to_string();
        self.turn.fail(error.clone());
        self.pending.push_back(AgentEvent::Error(error));
        self.finished = true;
    }

    fn handle_tool_calls(&mut self, tool_calls: Vec<ToolCall>) {
        if self.tool_rounds >= self.max_tool_rounds {
            self.fail_turn(format!(
                "tool call round limit exceeded ({})",
                self.max_tool_rounds
            ));
            return;
        }
        if tool_calls.is_empty() {
            self.fail_turn("model requested tool_calls but did not provide any tool call");
            return;
        }

        if let Some(step) = self.turn.steps.last_mut() {
            step.complete();
        }
        self.tool_rounds += 1;
        self.assistant_text.clear();
        self.assistant_deltas.clear();

        let assistant_message = Message::assistant_tool_calls(tool_calls.clone());
        self.conversation.push(assistant_message.clone());
        self.turn_messages.push(assistant_message);
        self.pending_tool_calls = VecDeque::from(tool_calls);
        self.start_next_tool_call();
    }

    fn start_next_tool_call(&mut self) {
        let Some(tool_call) = self.pending_tool_calls.pop_front() else {
            self.start_next_model_call();
            return;
        };

        let id = tool_call.id.clone();
        let name = tool_call.function.name.clone();
        self.turn
            .steps
            .push(TurnStep::running_tool_call(name.clone(), id.clone()));
        self.pending.push_back(AgentEvent::ToolCallStarted {
            id: id.clone(),
            name: name.clone(),
        });

        let tools = self.tools.clone();
        let call_for_result = tool_call.clone();
        self.tool_future = Some(
            async move {
                let call_for_execution = call_for_result.clone();
                let execution =
                    tokio::task::spawn_blocking(move || tools.execute(&call_for_execution))
                        .await
                        .unwrap_or_else(|err| {
                            ToolExecution::error(format!("tool execution task failed: {err}"))
                        });
                (call_for_result, execution)
            }
            .boxed(),
        );
    }

    fn start_approved_tool_call(
        &mut self,
        tool_call: ToolCall,
        decision: ApprovalDecision,
        request: ApprovalRequest,
    ) {
        let tools = self.tools.clone();
        let call_for_result = tool_call.clone();
        self.tool_future = Some(
            async move {
                let call_for_execution = call_for_result.clone();
                let execution = tokio::task::spawn_blocking(move || {
                    tools.execute_approved(&call_for_execution, &decision, &request)
                })
                .await
                .unwrap_or_else(|err| {
                    ToolExecution::error(format!("tool execution task failed: {err}"))
                });
                (call_for_result, execution)
            }
            .boxed(),
        );
    }

    fn finish_tool_execution(&mut self, tool_call: ToolCall, execution: ToolExecution) {
        match execution {
            ToolExecution::Completed(result) => self.finish_tool_call(tool_call, result),
            ToolExecution::ApprovalRequired(request) => {
                self.pending_approval = Some(PendingApproval {
                    tool_call,
                    request: request.clone(),
                });
                self.pending
                    .push_back(AgentEvent::ApprovalRequested(request));
            }
        }
    }

    fn finish_tool_call(&mut self, tool_call: ToolCall, result: ToolResult) {
        let id = tool_call.id.clone();
        let name = tool_call.function.name.clone();
        let ok = result.ok;
        let error = result.error.clone();
        let summary = result.summary.clone();
        let tool_message = Message::tool_result(id.clone(), result.content);
        self.conversation.push(tool_message.clone());
        self.turn_messages.push(tool_message);

        if let Some(step) = self.turn.steps.last_mut() {
            if ok {
                step.complete();
            } else {
                step.fail(error.unwrap_or_else(|| "tool call failed".to_string()));
            }
        }

        self.pending.push_back(AgentEvent::ToolCallFinished {
            id,
            name,
            ok,
            summary,
        });
        self.start_next_tool_call();
    }

    fn start_next_model_call(&mut self) {
        self.turn.steps.push(TurnStep::running_model_call());
        let client = self.client.clone();
        let conversation = self.conversation.clone();
        let tools = self.tools.definitions().to_vec();
        self.model_start =
            Some(async move { client.stream_chat(&conversation, &tools).await }.boxed());
    }
}

impl Unpin for AgentTurnStream<'_> {}

impl Stream for AgentTurnStream<'_> {
    type Item = AgentEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        if this.finished {
            return Poll::Ready(None);
        }

        loop {
            if let Some(future) = this.tool_future.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready((tool_call, execution)) => {
                        this.tool_future = None;
                        this.finish_tool_execution(tool_call, execution);
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if this.pending_approval.is_some() {
                return Poll::Pending;
            }

            if let Some(future) = this.model_start.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(model_stream)) => {
                        this.model_start = None;
                        this.model_stream = Some(model_stream);
                        continue;
                    }
                    Poll::Ready(Err(err)) => {
                        this.model_start = None;
                        this.fail_turn(err);
                        return Poll::Ready(this.pending.pop_front());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if let Some(model_stream) = this.model_stream.as_mut() {
                match Pin::new(model_stream).poll_next(cx) {
                    Poll::Ready(Some(Ok(ModelEvent::TextDelta(text)))) => {
                        this.assistant_text.push_str(&text);
                        this.assistant_deltas.push_back(text);
                        continue;
                    }
                    Poll::Ready(Some(Ok(ModelEvent::ToolCalls(tool_calls)))) => {
                        this.model_stream = None;
                        this.handle_tool_calls(tool_calls);
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    Poll::Ready(Some(Ok(ModelEvent::Completed))) => {
                        this.model_stream = None;
                        this.complete_turn();
                        return Poll::Ready(this.pending.pop_front());
                    }
                    Poll::Ready(Some(Err(err))) => {
                        this.model_stream = None;
                        this.fail_turn(err);
                        return Poll::Ready(this.pending.pop_front());
                    }
                    Poll::Ready(None) => {
                        this.model_stream = None;
                        this.fail_turn("model stream ended before completion");
                        return Poll::Ready(this.pending.pop_front());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            this.fail_turn("agent turn has no active model or tool work");
            return Poll::Ready(this.pending.pop_front());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::OpenAiCompatConfig;
    use agent_protocol::{
        ApprovalAction, ApprovalDecision, PermissionMode, PermissionProfile, ToolCallKind,
        TurnStatus, TurnStepKind,
    };
    use agent_tools::ToolRegistry;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_sse_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}/v1")
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
        })
        .expect("client")
    }

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-core-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
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

    fn tools(root: &Path) -> ToolRegistry {
        ToolRegistry::built_in(
            root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        )
        .expect("tools")
    }

    async fn collect_events(mut stream: AgentTurnStream<'_>) -> (Vec<AgentEvent>, Turn) {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        let turn = stream.into_turn();
        (events, turn)
    }

    async fn next_event(stream: &mut AgentTurnStream<'_>) -> AgentEvent {
        stream.next().await.expect("next agent event")
    }

    #[tokio::test]
    async fn run_turn_emits_events_and_updates_thread() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let base_url = spawn_sse_server(body).await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "Say hi")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(
            events,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::TextDelta("Hello".to_string()),
                AgentEvent::TextDelta(" world".to_string()),
                AgentEvent::AgentMessage("Hello world".to_string()),
                AgentEvent::TurnCompleted,
            ]
        );

        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(
            turn.assistant_message,
            Some(Message::assistant("Hello world"))
        );
        assert_eq!(turn.steps[0].status, TurnStatus::Completed);
        assert_eq!(
            thread.messages,
            vec![Message::user("Say hi"), Message::assistant("Hello world"),]
        );
    }

    #[tokio::test]
    async fn run_turn_sends_prior_thread_messages_to_second_model_call() {
        let first_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"First answer\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Second answer\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "First question")
            .await
            .expect("first turn");
        let _ = collect_events(stream).await;
        let stream = agent
            .run_turn(&mut thread, "Second question")
            .await
            .expect("second turn");
        let _ = collect_events(stream).await;

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(r#""content":"First question""#));
        assert!(!requests[0].contains(r#""content":"First answer""#));
        assert!(requests[1].contains(r#""content":"You are helpful.""#));
        assert!(requests[1].contains(r#""content":"First question""#));
        assert!(requests[1].contains(r#""content":"First answer""#));
        assert!(requests[1].contains(r#""content":"Second question""#));
        assert_eq!(
            thread.messages,
            vec![
                Message::user("First question"),
                Message::assistant("First answer"),
                Message::user("Second question"),
                Message::assistant("Second answer"),
            ]
        );
    }

    #[tokio::test]
    async fn failed_turn_emits_error_and_does_not_update_thread() {
        let base_url = spawn_sse_server("data: {not-json}\n\n").await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();
        thread.push(Message::user("Earlier question"));
        thread.push(Message::assistant("Earlier answer"));

        let stream = agent
            .run_turn(&mut thread, "Broken question")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(events.len(), 2);
        assert_eq!(events[0], AgentEvent::TurnStarted);
        assert!(matches!(events[1], AgentEvent::Error(_)));
        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(turn.error.is_some());
        assert_eq!(turn.steps[0].status, TurnStatus::Failed);
        assert_eq!(
            thread.messages,
            vec![
                Message::user("Earlier question"),
                Message::assistant("Earlier answer"),
            ]
        );
    }

    #[tokio::test]
    async fn run_turn_executes_tool_calls_and_sends_results_to_next_model_call() {
        let root = unique_dir("tool-success");
        fs::write(root.join("note.txt"), "tool result\n").expect("write note");
        let first_body = tool_call_body(
            "call_1",
            "read_file",
            json!({"path": "note.txt", "max_lines": 5}),
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Read it\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "Read note.txt")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(
            events,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::ToolCallStarted {
                    id: "call_1".to_string(),
                    name: "read_file".to_string()
                },
                AgentEvent::ToolCallFinished {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    ok: true,
                    summary: None
                },
                AgentEvent::TextDelta("Read it".to_string()),
                AgentEvent::AgentMessage("Read it".to_string()),
                AgentEvent::TurnCompleted,
            ]
        );
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.steps.len(), 3);
        assert_eq!(turn.steps[0].kind, TurnStepKind::ModelCall);
        assert_eq!(turn.steps[1].kind, TurnStepKind::ToolCall);
        assert_eq!(turn.steps[1].tool_name.as_deref(), Some("read_file"));
        assert_eq!(turn.steps[2].kind, TurnStepKind::ModelCall);
        assert_eq!(thread.messages.len(), 4);
        assert_eq!(thread.messages[0], Message::user("Read note.txt"));
        assert_eq!(
            thread.messages[1].tool_calls.as_ref().expect("tool calls")[0].kind,
            ToolCallKind::Function
        );
        assert_eq!(thread.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(thread.messages[3], Message::assistant("Read it"));

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(r#""tools":[{"type":"function""#));
        assert!(requests[1].contains(r#""role":"tool""#));
        assert!(requests[1].contains(r#""tool_call_id":"call_1""#));
        assert!(requests[1].contains("tool result"));
    }

    #[tokio::test]
    async fn tool_errors_are_returned_to_model_without_failing_turn() {
        let root = unique_dir("tool-error");
        let first_body = tool_call_body("call_1", "read_file", json!({"path": "missing.txt"}));
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Missing\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "Read missing.txt")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(matches!(
            events.as_slice(),
            [
                AgentEvent::TurnStarted,
                AgentEvent::ToolCallStarted { .. },
                AgentEvent::ToolCallFinished { ok: false, .. },
                AgentEvent::TextDelta(_),
                AgentEvent::AgentMessage(_),
                AgentEvent::TurnCompleted,
            ]
        ));
        assert_eq!(turn.steps[1].status, TurnStatus::Failed);
        assert_eq!(thread.messages.len(), 4);

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains(r#"\"ok\":false"#));
        assert!(requests[1].contains("missing.txt"));
    }

    #[tokio::test]
    async fn shell_tool_approval_denial_is_returned_to_model() {
        let root = unique_dir("shell-approval-denied");
        let first_body = tool_call_body(
            "call_1",
            "shell_command",
            json!({"command": "pwd", "timeout_secs": 5}),
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Denied\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let mut stream = agent
            .run_turn(&mut thread, "Run pwd")
            .await
            .expect("run turn");

        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ToolCallStarted {
                id: "call_1".to_string(),
                name: "shell_command".to_string()
            }
        );
        let AgentEvent::ApprovalRequested(request) = next_event(&mut stream).await else {
            panic!("expected approval request");
        };

        stream
            .resolve_approval(ApprovalDecision::deny(request.id.clone()))
            .expect("resolve approval");

        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ApprovalResolved(ApprovalDecision::deny(request.id))
        );
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ToolCallFinished {
                id: "call_1".to_string(),
                name: "shell_command".to_string(),
                ok: false,
                summary: Some(agent_protocol::ToolExecutionSummary::error(
                    "approval denied"
                )),
            }
        );
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::TextDelta("Denied".to_string())
        );
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::AgentMessage("Denied".to_string())
        );
        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnCompleted);
        assert_eq!(stream.next().await, None);

        let turn = stream.into_turn();
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.steps[1].status, TurnStatus::Failed);
        assert_eq!(thread.messages.len(), 4);

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("approval denied"));
    }

    #[tokio::test]
    async fn file_change_approval_success_writes_file_and_emits_summary() {
        let root = unique_dir("file-approval-approved");
        let first_body = tool_call_body(
            "call_1",
            "write_file",
            json!({"path": "note.txt", "content": "created\n"}),
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Wrote it\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let mut stream = agent
            .run_turn(&mut thread, "Write note.txt")
            .await
            .expect("run turn");

        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ToolCallStarted {
                id: "call_1".to_string(),
                name: "write_file".to_string()
            }
        );
        let AgentEvent::ApprovalRequested(request) = next_event(&mut stream).await else {
            panic!("expected approval request");
        };
        let ApprovalAction::FileChanges { files, diff } = &request.action else {
            panic!("expected file changes approval");
        };
        assert_eq!(files.len(), 1);
        assert!(diff.contains("+created"));

        stream
            .resolve_approval(ApprovalDecision::approve(request.id.clone()))
            .expect("resolve approval");

        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ApprovalResolved(ApprovalDecision::approve(request.id))
        );
        let AgentEvent::ToolCallFinished {
            name,
            ok,
            summary: Some(summary),
            ..
        } = next_event(&mut stream).await
        else {
            panic!("expected summarized tool finish");
        };
        assert_eq!(name, "write_file");
        assert!(ok);
        assert_eq!(summary.files.len(), 1);
        assert!(summary.diff.as_deref().expect("diff").contains("+created"));
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::TextDelta("Wrote it".to_string())
        );
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::AgentMessage("Wrote it".to_string())
        );
        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnCompleted);
        assert_eq!(stream.next().await, None);

        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read created"),
            "created\n"
        );
        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains(r#""role":"tool""#));
    }

    #[tokio::test]
    async fn file_change_approval_denial_is_returned_to_model_without_writing() {
        let root = unique_dir("file-approval-denied");
        let first_body = tool_call_body(
            "call_1",
            "write_file",
            json!({"path": "note.txt", "content": "created\n"}),
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Denied\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let mut stream = agent
            .run_turn(&mut thread, "Write note.txt")
            .await
            .expect("run turn");

        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
        assert!(matches!(
            next_event(&mut stream).await,
            AgentEvent::ToolCallStarted { .. }
        ));
        let AgentEvent::ApprovalRequested(request) = next_event(&mut stream).await else {
            panic!("expected approval request");
        };
        stream
            .resolve_approval(ApprovalDecision::deny(request.id.clone()))
            .expect("resolve approval");

        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::ApprovalResolved(ApprovalDecision::deny(request.id))
        );
        assert!(matches!(
            next_event(&mut stream).await,
            AgentEvent::ToolCallFinished {
                name,
                ok: false,
                ..
            } if name == "write_file"
        ));
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::TextDelta("Denied".to_string())
        );
        assert_eq!(
            next_event(&mut stream).await,
            AgentEvent::AgentMessage("Denied".to_string())
        );
        assert_eq!(next_event(&mut stream).await, AgentEvent::TurnCompleted);
        assert_eq!(stream.next().await, None);

        assert!(!root.join("note.txt").exists());
        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("approval denied"));
    }

    #[tokio::test]
    async fn too_many_tool_rounds_fails_without_updating_thread() {
        let root = unique_dir("tool-limit");
        let bodies = (0..=DEFAULT_MAX_TOOL_ROUNDS)
            .map(|index| {
                tool_call_body(
                    &format!("call_{index}"),
                    "list_files",
                    json!({"path": ".", "max_entries": 1}),
                )
            })
            .collect::<Vec<_>>();
        let (base_url, requests) = spawn_recording_sse_server(bodies).await;
        let agent = Agent::with_tools(client(base_url), "You are helpful.", tools(&root));
        let mut thread = Thread::new();

        let stream = agent.run_turn(&mut thread, "Loop").await.expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(
            turn.error
                .as_deref()
                .expect("error")
                .contains("tool call round limit exceeded")
        );
        assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
        assert_eq!(thread.messages, Vec::<Message>::new());

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), DEFAULT_MAX_TOOL_ROUNDS + 1);
    }
}
