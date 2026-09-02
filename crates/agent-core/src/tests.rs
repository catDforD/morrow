use super::*;
use agent_protocol::{
    ApprovalAction, ApprovalDecision, FileChangeOperation, FileChangeSummary, PermissionMode,
    ToolCallKind, TurnStatus, TurnStepKind,
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct TestModelError(String);

impl fmt::Display for TestModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl StdError for TestModelError {}

enum ScriptedResponse {
    Events(Vec<Result<ModelEvent, String>>),
    Gated {
        first: Vec<Result<ModelEvent, String>>,
        rest: Vec<Result<ModelEvent, String>>,
        release: tokio::sync::oneshot::Receiver<()>,
    },
}

#[derive(Clone)]
struct ScriptedModel {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedModel {
    fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn recorded_requests(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.requests)
    }
}

impl Model for ScriptedModel {
    fn stream(&self, request: ModelRequest) -> ModelFuture {
        let messages = serde_json::to_string(&request.conversation.messages)
            .expect("serialize model messages");
        let tools = serde_json::to_string(&request.tools).expect("serialize model tools");
        let serialized = format!(r#"{{"messages":{messages},"tools":{tools}}}"#);
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(serialized);
        let response = self
            .responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .unwrap_or_else(|| {
                ScriptedResponse::Events(vec![Err(
                    "scripted model has no remaining response".to_string()
                )])
            });

        async move {
            let stream: ModelStream = match response {
                ScriptedResponse::Events(events) => {
                    stream::iter(events.into_iter().map(model_result)).boxed()
                }
                ScriptedResponse::Gated {
                    first,
                    rest,
                    release,
                } => {
                    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                    tokio::spawn(async move {
                        for event in first {
                            let _ = sender.send(model_result(event));
                        }
                        let _ = release.await;
                        for event in rest {
                            let _ = sender.send(model_result(event));
                        }
                    });
                    stream::unfold(receiver, |mut receiver| async move {
                        receiver.recv().await.map(|event| (event, receiver))
                    })
                    .boxed()
                }
            };
            Ok(stream)
        }
        .boxed()
    }
}

fn model_result(event: Result<ModelEvent, String>) -> Result<ModelEvent, ModelFailure> {
    event.map_err(|error| ModelFailure::new(TestModelError(error)))
}

async fn spawn_sse_server(body: &'static str) -> ScriptedModel {
    ScriptedModel::new(vec![ScriptedResponse::Events(parse_sse_body(body))])
}

async fn spawn_gated_sse_server(
    first_chunk: &'static str,
    rest: &'static str,
) -> (ScriptedModel, tokio::sync::oneshot::Sender<()>) {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let model = ScriptedModel::new(vec![ScriptedResponse::Gated {
        first: parse_sse_body(first_chunk),
        rest: parse_sse_body(rest),
        release: release_rx,
    }]);
    (model, release_tx)
}

async fn spawn_recording_sse_server(
    bodies: Vec<&'static str>,
) -> (ScriptedModel, Arc<Mutex<Vec<String>>>) {
    let responses = bodies
        .into_iter()
        .map(|body| ScriptedResponse::Events(parse_sse_body(body)))
        .collect();
    let model = ScriptedModel::new(responses);
    let requests = model.recorded_requests();
    (model, requests)
}

fn client(model: ScriptedModel) -> ScriptedModel {
    model
}

fn parse_sse_body(body: &str) -> Vec<Result<ModelEvent, String>> {
    let mut events = Vec::new();
    let mut tool_calls = Vec::new();
    for frame in body.replace("\r\n", "\n").split("\n\n") {
        let Some(data) = frame.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            events.push(Ok(ModelEvent::Completed));
            continue;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                events.push(Err(format!("failed to parse model stream JSON: {error}")));
                break;
            }
        };
        let Some(choice) = value["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        if let Some(content) = choice["delta"]["content"].as_str()
            && !content.is_empty()
        {
            events.push(Ok(ModelEvent::TextDelta(content.to_string())));
        }
        if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
            for call in calls {
                tool_calls.push(ToolCall::function(
                    call["id"].as_str().unwrap_or_default(),
                    call["function"]["name"].as_str().unwrap_or_default(),
                    call["function"]["arguments"].as_str().unwrap_or_default(),
                ));
            }
        }
        if choice["finish_reason"].as_str() == Some("tool_calls") {
            events.push(Ok(ModelEvent::ToolCalls(std::mem::take(&mut tool_calls))));
        }
    }
    events
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

fn tool_calls_body(calls: Vec<(&str, &str, serde_json::Value)>) -> &'static str {
    let tool_calls = calls
        .into_iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments.to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": tool_calls
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

#[derive(Debug, Clone)]
struct TestTools {
    root: PathBuf,
    mode: PermissionMode,
}

impl ToolRuntime for TestTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["read_file", "list_files", "write_file", "shell_command"]
            .into_iter()
            .map(|name| ToolDefinition::function(name, format!("Test tool {name}"), json!({})))
            .collect()
    }

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        match call.function.name.as_str() {
            "write_file" | "shell_command" => ToolExecutionMode::Serial,
            _ => ToolExecutionMode::Concurrent,
        }
    }

    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        let tools = self.clone();
        async move { tools.execute_now(call, approval) }.boxed()
    }
}

struct SubagentTestTools;

impl ToolRuntime for SubagentTestTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::function(
            "delegate_task",
            "delegate a test task",
            json!({}),
        )]
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execution_kind(&self, _call: &ToolCall) -> ToolExecutionKind {
        ToolExecutionKind::Subagent {
            task: "Inspect runtime".to_string(),
            identity: SubagentIdentity {
                id: "builtin-01".to_string(),
                name: "后藤一里".to_string(),
            },
        }
    }

    fn execute(
        &self,
        _call: ToolCall,
        _approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        async {
            let subagent = SubagentExecutionSummary::success(
                "Inspect runtime",
                "Runtime uses a reusable turn helper.",
                2,
                1,
                false,
            )
            .with_agent_identity(&SubagentIdentity {
                id: "builtin-01".to_string(),
                name: "后藤一里".to_string(),
            });
            ToolExecution::Completed(ToolResult {
                ok: true,
                content: serde_json::to_string(&json!({
                    "ok": true,
                    "agent_id": &subagent.agent_id,
                    "agent_name": &subagent.agent_name,
                    "task": &subagent.task,
                    "result": &subagent.result,
                    "model_calls": subagent.model_calls,
                    "tool_calls": subagent.tool_calls,
                    "truncated": subagent.truncated,
                }))
                .expect("subagent output"),
                error: None,
                summary: Some(ToolExecutionSummary::subagent(subagent)),
            })
        }
        .boxed()
    }
}

impl TestTools {
    fn execute_now(&self, call: ToolCall, approval: Option<ToolApproval>) -> ToolExecution {
        let arguments: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolExecution::error(format!("invalid arguments: {error}")),
        };
        match call.function.name.as_str() {
            "read_file" => self.read_file(arguments),
            "list_files" => self.list_files(arguments),
            "write_file" => self.write_file(call.id, arguments, approval),
            "shell_command" => self.shell_command(call.id, arguments, approval),
            name => ToolExecution::error(format!("unknown tool {name:?}")),
        }
    }

    fn read_file(&self, arguments: Value) -> ToolExecution {
        let path = arguments["path"].as_str().unwrap_or_default();
        match fs::read_to_string(self.root.join(path)) {
            Ok(content) => completed_ok(json!({ "path": path, "content": content }), None),
            Err(error) => ToolExecution::error(format!("failed to read {path}: {error}")),
        }
    }

    fn list_files(&self, arguments: Value) -> ToolExecution {
        let path = arguments["path"].as_str().unwrap_or(".");
        let entries = fs::read_dir(self.root.join(path))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        completed_ok(json!({ "path": path, "entries": entries }), None)
    }

    fn write_file(
        &self,
        call_id: String,
        arguments: Value,
        approval: Option<ToolApproval>,
    ) -> ToolExecution {
        let path = arguments["path"].as_str().unwrap_or_default();
        let content = arguments["content"].as_str().unwrap_or_default();
        let summary = FileChangeSummary {
            path: path.to_string(),
            operation: FileChangeOperation::Add,
            replacements: 0,
            created: true,
            overwritten: false,
            deleted: false,
        };
        let added = content
            .lines()
            .map(|line| format!("+{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = format!("--- /dev/null\n+++ {path}\n{added}\n");

        if self.mode != PermissionMode::DangerFullAccess && approval.is_none() {
            return ToolExecution::ApprovalRequired(ApprovalRequest::file_changes(
                format!("approval-{call_id}"),
                vec![summary],
                diff,
                "file change requires approval",
            ));
        }
        if let Err(error) = fs::write(self.root.join(path), content) {
            return ToolExecution::error(format!("failed to write {path}: {error}"));
        }
        completed_ok(
            json!({ "path": path }),
            Some(ToolExecutionSummary::file_changes(vec![summary], diff)),
        )
    }

    fn shell_command(
        &self,
        call_id: String,
        arguments: Value,
        approval: Option<ToolApproval>,
    ) -> ToolExecution {
        let command = arguments["command"].as_str().unwrap_or_default();
        let timeout_secs = arguments["timeout_secs"].as_u64().unwrap_or(30);
        if approval.is_none() {
            return ToolExecution::ApprovalRequired(ApprovalRequest::shell_command(
                format!("approval-{call_id}"),
                command,
                &self.root,
                timeout_secs,
                "shell command requires approval",
            ));
        }
        completed_ok(json!({ "command": command }), None)
    }
}

fn completed_ok(data: Value, summary: Option<ToolExecutionSummary>) -> ToolExecution {
    ToolExecution::Completed(ToolResult {
        ok: true,
        content: serde_json::to_string(&json!({ "ok": true, "data": data }))
            .expect("serialize tool result"),
        error: None,
        summary,
    })
}

fn tools(root: &Path) -> TestTools {
    tools_with_permissions(root, PermissionMode::WorkspaceWrite)
}

fn tools_with_permissions(root: &Path, mode: PermissionMode) -> TestTools {
    TestTools {
        root: root.to_path_buf(),
        mode,
    }
}

#[derive(Clone)]
struct CancellationProbeTools {
    observed: Arc<Mutex<Vec<bool>>>,
}

impl ToolRuntime for CancellationProbeTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::function(
            "probe",
            "Test cancellation",
            json!({}),
        )]
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execute(
        &self,
        _call: ToolCall,
        _approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture {
        self.observed
            .lock()
            .expect("observed lock poisoned")
            .push(context.cancellation.is_cancelled());
        async { completed_ok(json!({ "observed": true }), None) }.boxed()
    }
}

#[derive(Clone)]
struct DropProbeTools {
    token: Arc<Mutex<Option<CancellationToken>>>,
}

impl ToolRuntime for DropProbeTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::function(
            "wait",
            "Never completes",
            json!({}),
        )]
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execute(
        &self,
        _call: ToolCall,
        _approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture {
        *self.token.lock().expect("token lock poisoned") = Some(context.cancellation);
        futures_util::future::pending().boxed()
    }
}

#[derive(Clone)]
struct OutOfOrderTools;

impl ToolRuntime for OutOfOrderTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["slow", "fast"]
            .into_iter()
            .map(|name| ToolDefinition::function(name, "Test ordering", json!({})))
            .collect()
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execute(
        &self,
        call: ToolCall,
        _approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        if call.function.name == "fast" {
            async { completed_ok(json!({ "completed": true }), None) }.boxed()
        } else {
            futures_util::future::pending().boxed()
        }
    }
}

fn apply_record(thread: &mut Thread, record: TurnRecord) -> Turn {
    let TurnRecord { turn, messages } = record;
    if turn.status == TurnStatus::Completed {
        thread.messages.extend(messages);
    }
    turn
}

async fn collect_events(
    mut stream: AgentTurnStream<'_>,
    thread: &mut Thread,
) -> (Vec<AgentEvent>, Turn) {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if !matches!(
            event,
            AgentEvent::ModelMessageCommitted { .. } | AgentEvent::ToolResultCommitted { .. }
        ) {
            events.push(event);
        }
    }
    let turn = apply_record(thread, stream.into_turn_record());
    (events, turn)
}

async fn collect_all_events(
    mut stream: AgentTurnStream<'_>,
    thread: &mut Thread,
) -> (Vec<AgentEvent>, Turn) {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let turn = apply_record(thread, stream.into_turn_record());
    (events, turn)
}

async fn next_event(stream: &mut AgentTurnStream<'_>) -> AgentEvent {
    loop {
        let event = stream.next().await.expect("next agent event");
        if !matches!(
            event,
            AgentEvent::ModelMessageCommitted { .. } | AgentEvent::ToolResultCommitted { .. }
        ) {
            return event;
        }
    }
}

#[tokio::test]
async fn cancellation_token_wakes_all_waiters() {
    let token = CancellationToken::new();
    let first = {
        let token = token.clone();
        tokio::spawn(async move { token.cancelled().await })
    };
    let second = {
        let token = token.clone();
        tokio::spawn(async move { token.cancelled().await })
    };

    tokio::task::yield_now().await;
    token.cancel();

    tokio::time::timeout(Duration::from_secs(1), async {
        first.await.expect("first waiter");
        second.await.expect("second waiter");
    })
    .await
    .expect("all cancellation waiters must wake");
}

#[test]
fn agent_defaults_to_ninety_nine_tool_rounds() {
    let model = ScriptedModel::new(Vec::new());
    let agent = Agent::new(&model, "system");

    assert_eq!(agent.max_tool_rounds, 99);
}

#[tokio::test]
async fn turn_stream_records_model_invocation_before_execution() {
    let model = ScriptedModel::new(Vec::new());
    let agent = Agent::new(&model, "system");
    let invocation = ModelInvocation {
        provider_id: "provider".to_string(),
        provider_name: "Provider".to_string(),
        model_id: "model".to_string(),
        model_name: "Model".to_string(),
        reasoning: agent_protocol::ReasoningLevel::High,
    };
    let mut stream = agent
        .run_turn(&Thread::new(), "hello")
        .await
        .expect("create turn stream");

    stream.set_model_invocation(invocation.clone());

    assert_eq!(stream.turn().model.as_ref(), Some(&invocation));
    stream.cancel();
}

#[tokio::test]
async fn run_turn_emits_events_and_updates_thread() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let base_url = spawn_sse_server(body).await;
    let model = client(base_url);
    let agent = Agent::new(&model, "You are helpful.");
    let mut thread = Thread::new();

    let stream = agent.run_turn(&thread, "Say hi").await.expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(
        events,
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
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
async fn subagent_tools_emit_semantic_events_and_return_results_to_model() {
    let model = ScriptedModel::new(vec![
        ScriptedResponse::Events(vec![Ok(ModelEvent::ToolCalls(vec![ToolCall::function(
            "call-1",
            "delegate_task",
            json!({"task": "Inspect runtime"}).to_string(),
        )]))]),
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::TextDelta("Used subagent result".to_string())),
            Ok(ModelEvent::Completed),
        ]),
    ]);
    let tools = SubagentTestTools;
    let agent = Agent::with_tools(&model, "system", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Research runtime")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
            AgentEvent::SubagentStarted {
                id: start_id,
                agent_id: Some(start_agent_id),
                agent_name: Some(start_name),
                task,
            },
            AgentEvent::SubagentFinished { id: finish_id, ok: true, summary },
            AgentEvent::ModelCallStarted,
            AgentEvent::TextDelta(_),
            AgentEvent::AgentMessage(_),
            AgentEvent::TurnCompleted,
        ] if start_id == "call-1"
            && finish_id == "call-1"
            && start_agent_id == "builtin-01"
            && start_name == "后藤一里"
            && task == "Inspect runtime"
            && summary.agent_name.as_deref() == Some("后藤一里")
            && summary.agent_id.as_deref() == Some("builtin-01")
            && summary.result.as_deref() == Some("Runtime uses a reusable turn helper.")
    ));
    assert_eq!(turn.status, TurnStatus::Completed);
    let requests = model.recorded_requests();
    let requests = requests.lock().expect("requests lock poisoned");
    assert!(requests[1].contains("Runtime uses a reusable turn helper."));
}

#[tokio::test]
async fn run_turn_emits_text_delta_before_stream_done() {
    let first_chunk =
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
    let rest = "data: [DONE]\n\n";
    let (base_url, release) = spawn_gated_sse_server(first_chunk, rest).await;
    let model = client(base_url);
    let agent = Agent::new(&model, "You are helpful.");
    let thread = Thread::new();
    let mut stream = agent.run_turn(&thread, "Say hi").await.expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
    let delta = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("text delta before done")
        .expect("text delta event");
    assert_eq!(delta, AgentEvent::TextDelta("Hello".to_string()));

    release.send(()).expect("release stream");

    assert_eq!(
        next_event(&mut stream).await,
        AgentEvent::AgentMessage("Hello".to_string())
    );
    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnCompleted);
    assert_eq!(stream.next().await, None);
}

#[tokio::test]
async fn cancelling_stream_returns_failed_turn_record() {
    let first_chunk =
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
    let rest = "data: [DONE]\n\n";
    let (model, _release) = spawn_gated_sse_server(first_chunk, rest).await;
    let agent = Agent::new(&model, "You are helpful.");
    let thread = Thread::new();
    let mut stream = agent.run_turn(&thread, "Say hi").await.expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
    assert_eq!(
        next_event(&mut stream).await,
        AgentEvent::TextDelta("Hello".to_string())
    );
    stream.cancel();

    assert_eq!(
        next_event(&mut stream).await,
        AgentEvent::Error("turn cancelled".to_string())
    );
    assert_eq!(stream.next().await, None);
    let record = stream.into_turn_record();
    assert_eq!(record.turn.status, TurnStatus::Failed);
    assert_eq!(record.turn.error.as_deref(), Some("turn cancelled"));
    assert_eq!(record.messages, vec![Message::user("Say hi")]);
}

#[tokio::test]
async fn reused_agent_creates_a_fresh_default_cancellation_context_per_turn() {
    let model = ScriptedModel::new(vec![
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::TextDelta("unused".to_string())),
            Ok(ModelEvent::Completed),
        ]),
        ScriptedResponse::Events(vec![Ok(ModelEvent::ToolCalls(vec![ToolCall::function(
            "call-1", "probe", "{}",
        )]))]),
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::TextDelta("done".to_string())),
            Ok(ModelEvent::Completed),
        ]),
    ]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let tools = CancellationProbeTools {
        observed: observed.clone(),
    };
    let agent = Agent::with_tools(&model, "test", &tools);

    let mut first = agent
        .run_turn(&Thread::new(), "cancel this")
        .await
        .expect("first turn");
    first.cancel();
    while first.next().await.is_some() {}

    let mut second = agent
        .run_turn(&Thread::new(), "run the tool")
        .await
        .expect("second turn");
    while second.next().await.is_some() {}

    assert_eq!(second.turn().status, TurnStatus::Completed);
    assert_eq!(
        *observed.lock().expect("observed lock poisoned"),
        vec![false]
    );
}

#[tokio::test]
async fn dropping_an_unfinished_stream_cancels_running_tools() {
    let model = ScriptedModel::new(vec![ScriptedResponse::Events(vec![Ok(
        ModelEvent::ToolCalls(vec![ToolCall::function("call-1", "wait", "{}")]),
    )])]);
    let token = Arc::new(Mutex::new(None));
    let tools = DropProbeTools {
        token: token.clone(),
    };
    let agent = Agent::with_tools(&model, "test", &tools);
    let mut stream = agent
        .run_turn(&Thread::new(), "start waiting")
        .await
        .expect("turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
    assert!(matches!(
        next_event(&mut stream).await,
        AgentEvent::ToolCallStarted { .. }
    ));
    let cancellation = token
        .lock()
        .expect("token lock poisoned")
        .clone()
        .expect("tool context token");

    drop(stream);

    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn duplicate_tool_call_ids_fail_before_any_tool_starts() {
    let model = ScriptedModel::new(vec![ScriptedResponse::Events(vec![Ok(
        ModelEvent::ToolCalls(vec![
            ToolCall::function("duplicate", "first", "{}"),
            ToolCall::function("duplicate", "second", "{}"),
        ]),
    )])]);
    let agent = Agent::new(&model, "test");
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "invalid tools")
        .await
        .expect("turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Failed);
    assert!(
        turn.error
            .as_deref()
            .is_some_and(|error| error.contains("duplicate tool call id"))
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::ToolCallStarted { .. }))
    );
    assert!(
        turn.steps
            .iter()
            .all(|step| step.status != TurnStatus::Running)
    );
    assert!(thread.messages.is_empty());
}

#[tokio::test]
async fn cancellation_preserves_an_out_of_order_completed_tool_step() {
    let model = ScriptedModel::new(vec![ScriptedResponse::Events(vec![Ok(
        ModelEvent::ToolCalls(vec![
            ToolCall::function("slow-call", "slow", "{}"),
            ToolCall::function("fast-call", "fast", "{}"),
        ]),
    )])]);
    let tools = OutOfOrderTools;
    let agent = Agent::with_tools(&model, "test", &tools);
    let mut stream = agent
        .run_turn(&Thread::new(), "run concurrently")
        .await
        .expect("turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
    assert!(matches!(
        next_event(&mut stream).await,
        AgentEvent::ToolCallStarted { .. }
    ));
    assert!(matches!(
        next_event(&mut stream).await,
        AgentEvent::ToolCallStarted { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stream.next())
            .await
            .is_err(),
        "the slow first call keeps ordered result emission pending"
    );

    let fast_step = stream
        .turn()
        .steps
        .iter()
        .find(|step| step.tool_call_id.as_deref() == Some("fast-call"))
        .expect("fast step");
    assert_eq!(fast_step.status, TurnStatus::Completed);

    stream.cancel();

    let slow_step = stream
        .turn()
        .steps
        .iter()
        .find(|step| step.tool_call_id.as_deref() == Some("slow-call"))
        .expect("slow step");
    let fast_step = stream
        .turn()
        .steps
        .iter()
        .find(|step| step.tool_call_id.as_deref() == Some("fast-call"))
        .expect("fast step");
    assert_eq!(slow_step.status, TurnStatus::Failed);
    assert_eq!(fast_step.status, TurnStatus::Completed);
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
    let model = client(base_url);
    let agent = Agent::new(&model, "You are helpful.");
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "First question")
        .await
        .expect("first turn");
    let _ = collect_events(stream, &mut thread).await;
    let stream = agent
        .run_turn(&thread, "Second question")
        .await
        .expect("second turn");
    let _ = collect_events(stream, &mut thread).await;

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
    let model = client(base_url);
    let agent = Agent::new(&model, "You are helpful.");
    let mut thread = Thread::new();
    thread.push(Message::user("Earlier question"));
    thread.push(Message::assistant("Earlier answer"));

    let stream = agent
        .run_turn(&thread, "Broken question")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(events.len(), 3);
    assert_eq!(events[0], AgentEvent::TurnStarted);
    assert_eq!(events[1], AgentEvent::ModelCallStarted);
    assert!(matches!(events[2], AgentEvent::Error(_)));
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read note.txt")
        .await
        .expect("run turn");
    let (events, turn) = collect_all_events(stream, &mut thread).await;

    assert_eq!(events.len(), 11);
    assert!(matches!(events[0], AgentEvent::TurnStarted));
    assert!(matches!(events[1], AgentEvent::ModelCallStarted));
    assert!(matches!(
        &events[2],
        AgentEvent::ModelMessageCommitted { message, .. }
            if message.tool_calls.as_ref().is_some_and(|calls| calls[0].id == "call_1")
    ));
    assert!(matches!(
        &events[3],
        AgentEvent::ToolCallStarted { id, name }
            if id == "call_1" && name == "read_file"
    ));
    assert!(matches!(
        &events[4],
        AgentEvent::ToolResultCommitted { tool_call_id, ok: true, .. }
            if tool_call_id == "call_1"
    ));
    assert!(matches!(
        &events[5],
        AgentEvent::ToolCallFinished { id, name, ok: true, summary: None }
            if id == "call_1" && name == "read_file"
    ));
    assert!(matches!(events[6], AgentEvent::ModelCallStarted));
    assert_eq!(events[7], AgentEvent::TextDelta("Read it".to_string()));
    assert!(matches!(
        events[8],
        AgentEvent::ModelMessageCommitted { .. }
    ));
    assert_eq!(events[9], AgentEvent::AgentMessage("Read it".to_string()));
    assert!(matches!(events[10], AgentEvent::TurnCompleted));
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
async fn reasoning_content_is_preserved_across_tool_rounds() {
    let root = unique_dir("reasoning-tool-round");
    fs::write(root.join("note.txt"), "tool result\n").expect("write note");
    let model = ScriptedModel::new(vec![
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::ReasoningDelta("inspect first".to_string())),
            Ok(ModelEvent::ToolCalls(vec![ToolCall::function(
                "call_1",
                "read_file",
                r#"{"path":"note.txt","max_lines":5}"#,
            )])),
        ]),
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::ReasoningDelta("use result".to_string())),
            Ok(ModelEvent::TextDelta("Read it".to_string())),
            Ok(ModelEvent::Completed),
        ]),
    ]);
    let requests = model.recorded_requests();
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read note.txt")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert!(events.contains(&AgentEvent::ReasoningDelta("inspect first".to_string())));
    assert!(events.contains(&AgentEvent::ReasoningDelta("use result".to_string())));
    assert_eq!(
        thread.messages[1].reasoning_content.as_deref(),
        Some("inspect first")
    );
    assert_eq!(
        thread.messages[3].reasoning_content.as_deref(),
        Some("use result")
    );
    assert_eq!(
        turn.assistant_message
            .as_ref()
            .and_then(|message| message.reasoning_content.as_deref()),
        Some("use result")
    );
    let requests = requests.lock().expect("requests lock poisoned");
    assert!(requests[1].contains(r#""reasoning_content":"inspect first""#));
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read missing.txt")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
            AgentEvent::ToolCallStarted { .. },
            AgentEvent::ToolCallFinished { ok: false, .. },
            AgentEvent::ModelCallStarted,
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
async fn run_turn_executes_multiple_tool_calls_in_order() {
    let root = unique_dir("multi-tool-success");
    fs::write(root.join("a.txt"), "alpha\n").expect("write a");
    fs::write(root.join("b.txt"), "bravo\n").expect("write b");
    let first_body = tool_calls_body(vec![
        (
            "call_1",
            "read_file",
            json!({"path": "a.txt", "max_lines": 5}),
        ),
        (
            "call_2",
            "read_file",
            json!({"path": "b.txt", "max_lines": 5}),
        ),
    ]);
    let second_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Read both\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read a.txt and b.txt")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.steps.len(), 4);
    assert_eq!(turn.steps[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(turn.steps[2].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(thread.messages.len(), 5);
    assert_eq!(
        thread.messages[1]
            .tool_calls
            .as_ref()
            .expect("tool calls")
            .len(),
        2
    );
    assert_eq!(thread.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(thread.messages[3].tool_call_id.as_deref(), Some("call_2"));
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
            AgentEvent::ToolCallStarted { id: first_id, .. },
            AgentEvent::ToolCallStarted { id: second_id, .. },
            AgentEvent::ToolCallFinished {
                id: first_finish,
                ok: true,
                ..
            },
            AgentEvent::ToolCallFinished {
                id: second_finish,
                ok: true,
                ..
            },
            AgentEvent::ModelCallStarted,
            AgentEvent::TextDelta(_),
            AgentEvent::AgentMessage(_),
            AgentEvent::TurnCompleted,
        ] if first_id == "call_1"
            && first_finish == "call_1"
            && second_id == "call_2"
            && second_finish == "call_2"
    ));

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(r#""tool_call_id":"call_1""#));
    assert!(requests[1].contains(r#""tool_call_id":"call_2""#));
    assert!(requests[1].contains("alpha"));
    assert!(requests[1].contains("bravo"));
}

#[tokio::test]
async fn serial_tool_call_drains_concurrent_batch_before_starting_next_tool() {
    let root = unique_dir("serial-tool-barrier");
    fs::write(root.join("a.txt"), "alpha\n").expect("write a");
    fs::write(root.join("b.txt"), "bravo\n").expect("write b");
    let first_body = tool_calls_body(vec![
        (
            "call_1",
            "read_file",
            json!({"path": "a.txt", "max_lines": 5}),
        ),
        (
            "call_2",
            "write_file",
            json!({"path": "created.txt", "content": "created\n"}),
        ),
        (
            "call_3",
            "read_file",
            json!({"path": "b.txt", "max_lines": 5}),
        ),
    ]);
    let second_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
    let model = client(base_url);
    let tools = tools_with_permissions(&root, PermissionMode::DangerFullAccess);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read, write, read")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        fs::read_to_string(root.join("created.txt")).expect("read created"),
        "created\n"
    );
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
            AgentEvent::ToolCallStarted { id: first_start, .. },
            AgentEvent::ToolCallFinished {
                id: first_finish,
                ok: true,
                ..
            },
            AgentEvent::ToolCallStarted { id: second_start, .. },
            AgentEvent::ToolCallFinished {
                id: second_finish,
                ok: true,
                ..
            },
            AgentEvent::ToolCallStarted { id: third_start, .. },
            AgentEvent::ToolCallFinished {
                id: third_finish,
                ok: true,
                ..
            },
            AgentEvent::ModelCallStarted,
            AgentEvent::TextDelta(_),
            AgentEvent::AgentMessage(_),
            AgentEvent::TurnCompleted,
        ] if first_start == "call_1"
            && first_finish == "call_1"
            && second_start == "call_2"
            && second_finish == "call_2"
            && third_start == "call_3"
            && third_finish == "call_3"
    ));
}

#[tokio::test]
async fn tool_call_after_text_delta_preserves_assistant_tool_message_content() {
    let root = unique_dir("tool-after-text");
    fs::write(root.join("note.txt"), "tool result\n").expect("write note");
    let tool_body = tool_call_body(
        "call_1",
        "read_file",
        json!({"path": "note.txt", "max_lines": 5}),
    );
    let first_body = Box::leak(
        format!(
            "data: {}\n\n{}",
            json!({
                "choices": [{
                    "delta": {
                        "content": "I will inspect it."
                    },
                    "finish_reason": null
                }]
            }),
            tool_body
        )
        .into_boxed_str(),
    );
    let second_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn(&thread, "Read note.txt")
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::TurnStarted,
            AgentEvent::ModelCallStarted,
            AgentEvent::TextDelta(prefix),
            AgentEvent::ToolCallStarted { .. },
            AgentEvent::ToolCallFinished { ok: true, .. },
            AgentEvent::ModelCallStarted,
            AgentEvent::TextDelta(done),
            AgentEvent::AgentMessage(_),
            AgentEvent::TurnCompleted,
        ] if prefix == "I will inspect it." && done == "Done"
    ));
    assert_eq!(
        thread.messages[1].content.as_deref(),
        Some("I will inspect it.")
    );
    assert_eq!(
        thread.messages[1]
            .tool_calls
            .as_ref()
            .expect("tool calls")
            .len(),
        1
    );
    assert_eq!(thread.messages[3], Message::assistant("Done"));

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(r#""content":"I will inspect it.""#));
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let mut thread = Thread::new();

    let mut stream = agent.run_turn(&thread, "Run pwd").await.expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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

    let turn = apply_record(&mut thread, stream.into_turn_record());
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.steps[1].status, TurnStatus::Failed);
    assert_eq!(thread.messages.len(), 4);

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("approval denied"));
}

#[tokio::test]
async fn approval_mismatch_keeps_pending_approval_until_correct_decision() {
    let root = unique_dir("approval-mismatch");
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let thread = Thread::new();
    let mut stream = agent
        .run_turn(&thread, "Write note.txt")
        .await
        .expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
    assert!(matches!(
        next_event(&mut stream).await,
        AgentEvent::ToolCallStarted { .. }
    ));
    let AgentEvent::ApprovalRequested(request) = next_event(&mut stream).await else {
        panic!("expected approval request");
    };

    let err = stream
        .resolve_approval(ApprovalDecision::approve("approval-wrong"))
        .expect_err("mismatched approval must fail");

    assert!(matches!(err, AgentError::Approval(_)));
    stream
        .resolve_approval(ApprovalDecision::deny(request.id.clone()))
        .expect("correct approval decision");
    assert_eq!(
        next_event(&mut stream).await,
        AgentEvent::ApprovalResolved(ApprovalDecision::deny(request.id))
    );
    assert!(matches!(
        next_event(&mut stream).await,
        AgentEvent::ToolCallFinished { ok: false, .. }
    ));
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 2);
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let thread = Thread::new();

    let mut stream = agent
        .run_turn(&thread, "Write note.txt")
        .await
        .expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    let model = client(base_url);
    let tools = tools(&root);
    let agent = Agent::with_tools(&model, "You are helpful.", &tools);
    let thread = Thread::new();

    let mut stream = agent
        .run_turn(&thread, "Write note.txt")
        .await
        .expect("run turn");

    assert_eq!(next_event(&mut stream).await, AgentEvent::TurnStarted);
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    assert_eq!(next_event(&mut stream).await, AgentEvent::ModelCallStarted);
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
    const TEST_MAX_TOOL_ROUNDS: usize = 3;
    let root = unique_dir("tool-limit");
    let bodies = (0..=TEST_MAX_TOOL_ROUNDS)
        .map(|index| {
            tool_call_body(
                &format!("call_{index}"),
                "list_files",
                json!({"path": ".", "max_entries": 1}),
            )
        })
        .collect::<Vec<_>>();
    let (base_url, requests) = spawn_recording_sse_server(bodies).await;
    let model = client(base_url);
    let tools = tools(&root);
    let mut agent = Agent::with_tools(&model, "You are helpful.", &tools);
    agent.max_tool_rounds = TEST_MAX_TOOL_ROUNDS;
    let mut thread = Thread::new();

    let stream = agent.run_turn(&thread, "Loop").await.expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

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
    assert_eq!(requests.len(), TEST_MAX_TOOL_ROUNDS + 1);
}

#[derive(Debug)]
struct BigOutputTools;

impl ToolRuntime for BigOutputTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::function(
            "big_read",
            "Reads a large blob",
            json!({}),
        )]
    }

    fn execution_mode(&self, _call: &ToolCall) -> ToolExecutionMode {
        ToolExecutionMode::Concurrent
    }

    fn execute(
        &self,
        _call: ToolCall,
        _approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolFuture {
        async {
            ToolExecution::Completed(ToolResult {
                ok: true,
                content: "x".repeat(8_000),
                error: None,
                summary: None,
            })
        }
        .boxed()
    }
}

fn big_output_turn_setup(
    second_response: ScriptedResponse,
) -> (ScriptedModel, BigOutputTools, Thread) {
    let model = ScriptedModel::new(vec![
        ScriptedResponse::Events(vec![Ok(ModelEvent::ToolCalls(vec![ToolCall::function(
            "call-1", "big_read", "{}",
        )]))]),
        second_response,
    ]);
    (model, BigOutputTools, Thread::new())
}

#[tokio::test]
async fn context_limit_triggers_wrap_up_call_without_tools() {
    let (model, tools, mut thread) = big_output_turn_setup(ScriptedResponse::Events(vec![
        Ok(ModelEvent::TextDelta("partial summary".to_string())),
        Ok(ModelEvent::Completed),
    ]));
    let requests = model.recorded_requests();
    let agent = Agent::with_tools(&model, "system", &tools);

    let stream = agent
        .run_turn_with_agent_context(
            &thread,
            "read the big blob",
            AgentRunContext {
                context_token_limit: Some(200),
                ..AgentRunContext::default()
            },
        )
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
    assert_eq!(
        thread.messages,
        vec![
            Message::user("read the big blob"),
            Message::assistant_tool_calls(vec![ToolCall::function("call-1", "big_read", "{}")]),
            Message::tool_result("call-1", "x".repeat(8_000)),
            Message::assistant("partial summary"),
        ]
    );

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(r#""name":"big_read""#));
    // 收尾调用不带工具，且 conversation 中注入了停止用工具的收尾指令。
    assert!(requests[1].contains(r#""tools":[]"#));
    assert!(requests[1].contains("context token limit"));
}

#[tokio::test]
async fn wrap_up_call_that_requests_tools_fails_turn() {
    let (model, tools, mut thread) = big_output_turn_setup(ScriptedResponse::Events(vec![Ok(
        ModelEvent::ToolCalls(vec![ToolCall::function("call-2", "big_read", "{}")]),
    )]));
    let requests = model.recorded_requests();
    let agent = Agent::with_tools(&model, "system", &tools);

    let stream = agent
        .run_turn_with_agent_context(
            &thread,
            "read the big blob",
            AgentRunContext {
                context_token_limit: Some(200),
                ..AgentRunContext::default()
            },
        )
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(
        turn.error.as_deref(),
        Some("context limit exceeded mid-turn")
    );
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    assert!(thread.messages.is_empty());

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(r#""tools":[]"#));
}

#[tokio::test]
async fn wrap_up_call_stream_error_reports_context_limit() {
    let (model, tools, mut thread) = big_output_turn_setup(ScriptedResponse::Events(vec![Err(
        "upstream provider returned 500".to_string(),
    )]));
    let agent = Agent::with_tools(&model, "system", &tools);

    let stream = agent
        .run_turn_with_agent_context(
            &thread,
            "read the big blob",
            AgentRunContext {
                context_token_limit: Some(200),
                ..AgentRunContext::default()
            },
        )
        .await
        .expect("run turn");
    let (_events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(
        turn.error.as_deref(),
        Some("context limit exceeded mid-turn")
    );
    assert!(thread.messages.is_empty());
}

#[tokio::test]
async fn context_limit_above_watermark_keeps_tools_available() {
    let (model, tools, mut thread) = big_output_turn_setup(ScriptedResponse::Events(vec![
        Ok(ModelEvent::TextDelta("done".to_string())),
        Ok(ModelEvent::Completed),
    ]));
    let requests = model.recorded_requests();
    let agent = Agent::with_tools(&model, "system", &tools);

    let stream = agent
        .run_turn_with_agent_context(
            &thread,
            "read the big blob",
            AgentRunContext {
                context_token_limit: Some(1_000_000),
                ..AgentRunContext::default()
            },
        )
        .await
        .expect("run turn");
    let (events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    // 水位未超限时后续调用仍带工具定义，也不注入收尾指令。
    assert!(requests[1].contains(r#""name":"big_read""#));
    assert!(!requests[1].contains("context token limit"));
}

struct ScriptedAfterTurn {
    id: &'static str,
    outputs: Mutex<Vec<AfterTurnOutput>>,
    final_texts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedAfterTurn {
    fn new(id: &'static str, outputs: Vec<AfterTurnOutput>) -> Self {
        Self {
            id,
            outputs: Mutex::new(outputs),
            final_texts: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AgentMiddleware for ScriptedAfterTurn {
    fn id(&self) -> &str {
        self.id
    }

    fn after_turn(&self, input: AfterTurnInput) -> Option<MiddlewareFuture<AfterTurnOutput>> {
        self.final_texts
            .lock()
            .expect("final_texts lock poisoned")
            .push(input.final_text);
        // 脚本用尽后重复最后一条，便于"持续打回"场景。
        let output = {
            let mut outputs = self.outputs.lock().expect("outputs lock poisoned");
            if outputs.len() > 1 {
                outputs.remove(0)
            } else {
                outputs[0].clone()
            }
        };
        Some(Box::pin(async move { Ok(output) }))
    }
}

fn after_turn_context() -> MiddlewareExecutionContext {
    MiddlewareExecutionContext {
        invocation_id: None,
        session: "test".to_string(),
        workspace_root: PathBuf::from("/workspace"),
        turn_index: 0,
        operation_id: None,
        turn_id: None,
        model: agent_protocol::ModelInvocation {
            provider_id: "test".to_string(),
            provider_name: "Test".to_string(),
            model_id: "model".to_string(),
            model_name: "Model".to_string(),
            reasoning: agent_protocol::ReasoningLevel::Off,
        },
        permissions: agent_protocol::PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: agent_protocol::ShellPolicy::Prompt,
        },
        agent_scope: agent_protocol::MiddlewareAgentScope::Main,
        cancellation: CancellationToken::new(),
    }
}

fn after_turn_run_context() -> AgentRunContext {
    AgentRunContext {
        middleware: Some(after_turn_context()),
        ..AgentRunContext::default()
    }
}

#[tokio::test]
async fn after_turn_continue_commits_assistant_and_reruns_model() {
    let model = ScriptedModel::new(vec![
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::TextDelta("first answer".to_string())),
            Ok(ModelEvent::Completed),
        ]),
        ScriptedResponse::Events(vec![
            Ok(ModelEvent::TextDelta("fixed answer".to_string())),
            Ok(ModelEvent::Completed),
        ]),
    ]);
    let requests = model.recorded_requests();
    let mut chain = AgentMiddlewareChain::new();
    chain.register(Arc::new(ScriptedAfterTurn::new(
        "verifier",
        vec![
            AfterTurnOutput::Continue {
                context: vec![ContextBlock::new("cargo test failed: auth_flow")],
            },
            AfterTurnOutput::Complete,
        ],
    )));
    let agent = Agent::new(&model, "system").with_middleware(chain);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn_with_agent_context(&thread, "fix it", after_turn_run_context())
        .await
        .expect("run turn");
    let (events, turn) = collect_all_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        turn.assistant_message,
        Some(Message::assistant("fixed answer"))
    );
    assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
    // 两次 after_turn 调用各产生一对 Started/Finished。
    let finished = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::MiddlewareFinished(_)))
        .count();
    assert_eq!(finished, 2);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::MiddlewareFinished(invocation)
            if invocation.stage == agent_protocol::MiddlewareStage::AfterTurn
    )));
    // 被打回的 assistant message 先进入消息链，再轮到新的 assistant 回复。
    assert_eq!(
        thread.messages,
        vec![
            Message::user("fix it"),
            Message::assistant("first answer"),
            Message::assistant("fixed answer"),
        ]
    );

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("first answer"));
    assert!(requests[1].contains("cargo test failed: auth_flow"));
}

#[tokio::test]
async fn after_turn_fail_fails_turn_with_attributed_reason() {
    let model = ScriptedModel::new(vec![ScriptedResponse::Events(vec![
        Ok(ModelEvent::TextDelta("done".to_string())),
        Ok(ModelEvent::Completed),
    ])]);
    let mut chain = AgentMiddlewareChain::new();
    chain.register(Arc::new(ScriptedAfterTurn::new(
        "verifier",
        vec![AfterTurnOutput::Fail {
            reason: "tests still red".to_string(),
        }],
    )));
    let agent = Agent::new(&model, "system").with_middleware(chain);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn_with_agent_context(&thread, "fix it", after_turn_run_context())
        .await
        .expect("run turn");
    let (events, turn) = collect_all_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(
        turn.error.as_deref(),
        Some("after-turn middleware rejected completion: verifier: tests still red")
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Error(error)) if error.contains("verifier: tests still red")
    ));
    assert!(thread.messages.is_empty());
}

#[tokio::test]
async fn after_turn_continue_limit_completes_turn_with_warning() {
    let responses = (1..=4)
        .map(|index| {
            ScriptedResponse::Events(vec![
                Ok(ModelEvent::TextDelta(format!("answer {index}"))),
                Ok(ModelEvent::Completed),
            ])
        })
        .collect();
    let model = ScriptedModel::new(responses);
    let requests = model.recorded_requests();
    let mut chain = AgentMiddlewareChain::new();
    chain.register(Arc::new(ScriptedAfterTurn::new(
        "verifier",
        vec![AfterTurnOutput::Continue {
            context: vec![ContextBlock::new("keep going")],
        }],
    )));
    let agent = Agent::new(&model, "system").with_middleware(chain);
    let mut thread = Thread::new();

    let stream = agent
        .run_turn_with_agent_context(&thread, "fix it", after_turn_run_context())
        .await
        .expect("run turn");
    let (events, turn) = collect_all_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.assistant_message, Some(Message::assistant("answer 4")));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Warning(warning) if warning.contains("continuation limit")
    )));
    assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 1 + MAX_AFTER_TURN_CONTINUES);
}

#[tokio::test]
async fn after_turn_without_middleware_context_completes_immediately() {
    let model = ScriptedModel::new(vec![ScriptedResponse::Events(vec![
        Ok(ModelEvent::TextDelta("done".to_string())),
        Ok(ModelEvent::Completed),
    ])]);
    let mut chain = AgentMiddlewareChain::new();
    let verifier = Arc::new(ScriptedAfterTurn::new(
        "verifier",
        vec![AfterTurnOutput::Fail {
            reason: "must not run".to_string(),
        }],
    ));
    let final_texts = verifier.final_texts.clone();
    chain.register(verifier);
    let agent = Agent::new(&model, "system").with_middleware(chain);
    let mut thread = Thread::new();

    // 无 middleware 执行上下文时保持原路径：after_turn 不运行，直接完成。
    let stream = agent.run_turn(&thread, "fix it").await.expect("run turn");
    let (_events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(final_texts.lock().expect("final_texts").is_empty());
}

#[tokio::test]
async fn after_turn_also_gates_wrap_up_completion() {
    let (model, tools, mut thread) = big_output_turn_setup(ScriptedResponse::Events(vec![
        Ok(ModelEvent::TextDelta("partial summary".to_string())),
        Ok(ModelEvent::Completed),
    ]));
    let mut chain = AgentMiddlewareChain::new();
    let verifier = Arc::new(ScriptedAfterTurn::new(
        "verifier",
        vec![AfterTurnOutput::Complete],
    ));
    let final_texts = verifier.final_texts.clone();
    chain.register(verifier);
    let agent = Agent::with_tools(&model, "system", &tools).with_middleware(chain);

    let mut run_context = after_turn_run_context();
    run_context.context_token_limit = Some(200);
    let stream = agent
        .run_turn_with_agent_context(&thread, "read the big blob", run_context)
        .await
        .expect("run turn");
    let (_events, turn) = collect_events(stream, &mut thread).await;

    assert_eq!(turn.status, TurnStatus::Completed);
    // 收尾模式的完成同样过 after_turn，且看到的是收尾文本。
    assert_eq!(
        *final_texts.lock().expect("final_texts"),
        vec!["partial summary".to_string()]
    );
}
