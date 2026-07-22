use agent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Conversation, Message, ModelInvocation,
    SubagentExecutionSummary, Thread, ToolCall, ToolDefinition, ToolExecutionSummary, Turn,
    TurnRecord, TurnStep,
};
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::{BoxStream, FuturesUnordered, Stream};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc as Shared, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};
use thiserror::Error;

const DEFAULT_MAX_TOOL_ROUNDS: usize = 99;
const MAX_CONCURRENT_TOOL_CALLS: usize = 4;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub conversation: Conversation,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCalls(Vec<ToolCall>),
    Completed,
}

type BoxError = Box<dyn StdError + Send + Sync + 'static>;

#[derive(Debug)]
pub struct ModelFailure {
    source: BoxError,
}

impl ModelFailure {
    pub fn new(error: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }
}

impl fmt::Display for ModelFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl StdError for ModelFailure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

pub type ModelStream = BoxStream<'static, Result<ModelEvent, ModelFailure>>;
pub type ModelFuture = BoxFuture<'static, Result<ModelStream, ModelFailure>>;

pub trait Model: Send + Sync {
    /// 返回拥有所有数据的 future，便于 turn 状态机跨多次 poll 持有模型请求。
    fn stream(&self, request: ModelRequest) -> ModelFuture;

    /// 返回可安全共享给隔离子任务的模型副本。不支持共享的实现保持默认 `None`。
    fn shared_clone(&self) -> Option<Shared<dyn Model>> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Concurrent,
    Serial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionKind {
    Standard,
    Subagent { task: String, agent_name: String },
}

#[derive(Debug, Clone)]
pub struct ToolApproval {
    pub decision: ApprovalDecision,
    pub request: ApprovalRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecution {
    Completed(ToolResult),
    ApprovalRequired(ApprovalRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub ok: bool,
    pub content: String,
    pub error: Option<String>,
    pub summary: Option<ToolExecutionSummary>,
}

impl ToolExecution {
    pub fn error(error: impl Into<String>) -> Self {
        Self::Completed(ToolResult::error(error))
    }
}

impl ToolResult {
    pub fn error(error: impl Into<String>) -> Self {
        let error = error.into();
        let content = serde_json::to_string(&serde_json::json!({
            "ok": false,
            "error": &error,
        }))
        .expect("tool error JSON must serialize");
        Self {
            ok: false,
            error: Some(error.clone()),
            content,
            summary: Some(ToolExecutionSummary::error(error)),
        }
    }
}

pub type ToolFuture = BoxFuture<'static, ToolExecution>;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    waiters: StdMutex<Vec<Waker>>,
}

/// 可在 runtime、核心状态机和工具适配器之间传递的轻量取消信号。
#[derive(Clone, Default)]
pub struct CancellationToken {
    state: Shared<CancellationState>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let waiters = {
            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        futures_util::future::poll_fn(|context| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }

            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            if !waiters
                .iter()
                .any(|waiter| waiter.will_wake(context.waker()))
            {
                waiters.push(context.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolExecutionContext {
    pub cancellation: CancellationToken,
}

pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode;

    fn execution_kind(&self, _call: &ToolCall) -> ToolExecutionKind {
        ToolExecutionKind::Standard
    }

    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture;
}

#[derive(Debug)]
struct EmptyToolRuntime;

impl ToolRuntime for EmptyToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
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
        let name = call.function.name;
        async move { ToolExecution::error(format!("unknown tool {name:?}")) }.boxed()
    }
}

static EMPTY_TOOL_RUNTIME: EmptyToolRuntime = EmptyToolRuntime;

#[derive(Clone)]
pub struct Agent<'a> {
    model: &'a dyn Model,
    system_prompt: String,
    tools: &'a dyn ToolRuntime,
    max_tool_rounds: usize,
}

impl fmt::Debug for Agent<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("system_prompt", &self.system_prompt)
            .field("tool_count", &self.tools.definitions().len())
            .field("max_tool_rounds", &self.max_tool_rounds)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("{0}")]
    Model(#[from] ModelFailure),
    #[error("{0}")]
    Approval(String),
}

impl<'a> Agent<'a> {
    pub fn new(model: &'a dyn Model, system_prompt: impl Into<String>) -> Self {
        Self {
            model,
            system_prompt: system_prompt.into(),
            tools: &EMPTY_TOOL_RUNTIME,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }

    pub fn with_tools(
        model: &'a dyn Model,
        system_prompt: impl Into<String>,
        tools: &'a dyn ToolRuntime,
    ) -> Self {
        Self {
            model,
            system_prompt: system_prompt.into(),
            tools,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }

    pub async fn run_turn<'b>(
        &'b self,
        thread: &Thread,
        prompt: impl Into<String>,
    ) -> Result<AgentTurnStream<'b>, AgentError> {
        self.run_turn_with_context(thread, prompt, ToolExecutionContext::default())
            .await
    }

    pub async fn run_turn_with_context<'b>(
        &'b self,
        thread: &Thread,
        prompt: impl Into<String>,
        tool_context: ToolExecutionContext,
    ) -> Result<AgentTurnStream<'b>, AgentError> {
        let user_message = Message::user(prompt.into());
        let mut conversation = Conversation::with_system_prompt(self.system_prompt.clone());
        conversation.messages.extend(thread.messages.clone());
        conversation.push(user_message.clone());
        // 工具定义在一个 turn 内保持不变，避免模型的后续调用看到不同的 schema。
        let tool_definitions = self.tools.definitions();
        let model_start = self.model.stream(ModelRequest {
            conversation: conversation.clone(),
            tools: tool_definitions.clone(),
        });

        Ok(AgentTurnStream {
            model: self.model,
            tools: self.tools,
            tool_context,
            tool_definitions,
            max_tool_rounds: self.max_tool_rounds,
            conversation,
            model_stream: None,
            model_start: Some(model_start),
            pending_tool_calls: VecDeque::new(),
            tool_futures: FuturesUnordered::new(),
            pending_tool_results: BTreeMap::new(),
            next_tool_result_index: 0,
            active_serial_tool: false,
            processing_tool_calls: false,
            pending_approval: None,
            turn: Turn::running(user_message.clone()),
            turn_messages: vec![user_message.clone()],
            assistant_reasoning: String::new(),
            assistant_text: String::new(),
            pending: VecDeque::from([AgentEvent::TurnStarted]),
            finished: false,
            tool_rounds: 0,
        })
    }
}

type ModelStartFuture = ModelFuture;
type ToolCallFuture = BoxFuture<'static, ToolCallOutcome>;

struct ToolCallOutcome {
    index: usize,
    tool_call: ToolCall,
    execution: ToolExecution,
    serial: bool,
}

#[derive(Debug, Clone)]
struct PendingApproval {
    index: usize,
    tool_call: ToolCall,
    request: ApprovalRequest,
}

pub struct AgentTurnStream<'a> {
    model: &'a dyn Model,
    tools: &'a dyn ToolRuntime,
    tool_context: ToolExecutionContext,
    tool_definitions: Vec<ToolDefinition>,
    max_tool_rounds: usize,
    conversation: Conversation,
    model_stream: Option<ModelStream>,
    model_start: Option<ModelStartFuture>,
    pending_tool_calls: VecDeque<(usize, ToolCall)>,
    tool_futures: FuturesUnordered<ToolCallFuture>,
    pending_tool_results: BTreeMap<usize, (ToolCall, ToolExecution)>,
    next_tool_result_index: usize,
    active_serial_tool: bool,
    processing_tool_calls: bool,
    pending_approval: Option<PendingApproval>,
    turn: Turn,
    turn_messages: Vec<Message>,
    assistant_reasoning: String,
    assistant_text: String,
    pending: VecDeque<AgentEvent>,
    finished: bool,
    tool_rounds: usize,
}

impl AgentTurnStream<'_> {
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn set_model_invocation(&mut self, model: ModelInvocation) {
        self.turn.model = Some(model);
    }

    pub fn into_turn(mut self) -> Turn {
        if !self.finished {
            self.cancel();
        }
        self.turn.clone()
    }

    pub fn into_turn_record(mut self) -> TurnRecord {
        if !self.finished {
            self.cancel();
        }
        TurnRecord::new(self.turn.clone(), self.turn_messages.clone())
    }

    /// 停止继续轮询模型和工具，并把当前 turn 作为失败记录收束。
    pub fn cancel(&mut self) {
        self.cancel_with_reason("turn cancelled");
    }

    pub fn cancel_with_reason(&mut self, error: impl ToString) {
        if self.finished {
            return;
        }

        self.tool_context.cancellation.cancel();
        self.model_start = None;
        self.model_stream = None;
        self.tool_futures = FuturesUnordered::new();
        self.pending_tool_calls.clear();
        self.pending_tool_results.clear();
        self.pending_approval = None;
        self.processing_tool_calls = false;
        self.pending.clear();
        self.fail_turn(error);
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
                pending_approval.index,
                pending_approval.tool_call,
                decision,
                pending_approval.request,
            );
        } else {
            let result = ToolResult::error("approval denied");
            self.finish_tool_call(pending_approval.tool_call, result);
            self.next_tool_result_index = pending_approval.index + 1;
            self.emit_ready_tool_results();
            self.start_ready_tool_calls();
            self.maybe_finish_tool_batch();
        }

        Ok(())
    }

    fn complete_turn(&mut self) {
        let assistant_text = self.assistant_text.clone();
        let assistant_message = Message::assistant(assistant_text.clone())
            .with_reasoning_content(self.assistant_reasoning.clone());
        self.turn_messages.push(assistant_message.clone());
        self.turn.complete(assistant_message);
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
        let mut ids = HashSet::with_capacity(tool_calls.len());
        for tool_call in &tool_calls {
            if tool_call.id.trim().is_empty() {
                self.fail_turn("model returned a tool call with an empty id");
                return;
            }
            let already_used = self
                .turn
                .steps
                .iter()
                .any(|step| step.tool_call_id.as_deref() == Some(tool_call.id.as_str()));
            if already_used || !ids.insert(tool_call.id.as_str()) {
                self.fail_turn(format!(
                    "model returned duplicate tool call id {:?}",
                    tool_call.id
                ));
                return;
            }
        }

        if let Some(step) = self.turn.steps.last_mut() {
            step.complete();
        }
        self.tool_rounds += 1;
        let assistant_message = if self.assistant_text.is_empty() {
            Message::assistant_tool_calls(tool_calls.clone())
        } else {
            Message::assistant_tool_calls_with_content(
                self.assistant_text.clone(),
                tool_calls.clone(),
            )
        }
        .with_reasoning_content(self.assistant_reasoning.clone());
        self.assistant_reasoning.clear();
        self.assistant_text.clear();
        self.conversation.push(assistant_message.clone());
        self.turn_messages.push(assistant_message);
        self.pending_tool_calls = tool_calls.into_iter().enumerate().collect();
        self.pending_tool_results.clear();
        self.next_tool_result_index = 0;
        self.active_serial_tool = false;
        self.processing_tool_calls = true;
        self.start_ready_tool_calls();
    }

    fn start_ready_tool_calls(&mut self) {
        if !self.processing_tool_calls || self.pending_approval.is_some() || self.active_serial_tool
        {
            return;
        }

        while self.tool_futures.len() < MAX_CONCURRENT_TOOL_CALLS {
            let Some((_, tool_call)) = self.pending_tool_calls.front() else {
                return;
            };
            let mode = self.tools.execution_mode(tool_call);
            let serial = mode == ToolExecutionMode::Serial;
            if serial && !self.tool_futures.is_empty() {
                return;
            }
            let (index, tool_call) = self
                .pending_tool_calls
                .pop_front()
                .expect("front pending tool call must exist");
            self.start_tool_call(index, tool_call, serial);
            if serial {
                return;
            }
        }
    }

    fn start_tool_call(&mut self, index: usize, tool_call: ToolCall, serial: bool) {
        let id = tool_call.id.clone();
        let name = tool_call.function.name.clone();
        self.turn
            .steps
            .push(TurnStep::running_tool_call(name.clone(), id.clone()));
        match self.tools.execution_kind(&tool_call) {
            ToolExecutionKind::Standard => {
                self.pending.push_back(AgentEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
            }
            ToolExecutionKind::Subagent { task, agent_name } => {
                self.pending.push_back(AgentEvent::SubagentStarted {
                    id: id.clone(),
                    agent_name: Some(agent_name),
                    task,
                });
            }
        }

        let call_for_result = tool_call.clone();
        let execution =
            self.tools
                .execute(call_for_result.clone(), None, self.tool_context.clone());
        if serial {
            self.active_serial_tool = true;
        }
        self.tool_futures.push(
            async move {
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    execution: execution.await,
                    serial,
                }
            }
            .boxed(),
        );
    }

    fn start_approved_tool_call(
        &mut self,
        index: usize,
        tool_call: ToolCall,
        decision: ApprovalDecision,
        request: ApprovalRequest,
    ) {
        let call_for_result = tool_call.clone();
        let execution = self.tools.execute(
            call_for_result.clone(),
            Some(ToolApproval { decision, request }),
            self.tool_context.clone(),
        );
        self.active_serial_tool = true;
        self.tool_futures.push(
            async move {
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    execution: execution.await,
                    serial: true,
                }
            }
            .boxed(),
        );
    }

    fn emit_ready_tool_results(&mut self) {
        while self.pending_approval.is_none() {
            let Some((tool_call, execution)) = self
                .pending_tool_results
                .remove(&self.next_tool_result_index)
            else {
                break;
            };
            match execution {
                ToolExecution::Completed(result) => {
                    self.finish_tool_call(tool_call, result);
                    self.next_tool_result_index += 1;
                }
                ToolExecution::ApprovalRequired(request) => {
                    self.pending_approval = Some(PendingApproval {
                        index: self.next_tool_result_index,
                        tool_call,
                        request: request.clone(),
                    });
                    self.pending
                        .push_back(AgentEvent::ApprovalRequested(request));
                }
            }
        }
    }

    fn maybe_finish_tool_batch(&mut self) {
        if self.processing_tool_calls
            && self.pending_tool_calls.is_empty()
            && self.tool_futures.is_empty()
            && self.pending_tool_results.is_empty()
            && self.pending_approval.is_none()
        {
            self.processing_tool_calls = false;
            self.start_next_model_call();
        }
    }

    fn finish_tool_execution(
        &mut self,
        index: usize,
        tool_call: ToolCall,
        execution: ToolExecution,
    ) {
        if let ToolExecution::Completed(result) = &execution {
            // 模型消息仍按原始 call 顺序回灌，但审计状态应在工具真实完成时更新。
            self.finish_tool_step(&tool_call, result);
        }
        self.pending_tool_results
            .insert(index, (tool_call, execution));
        self.emit_ready_tool_results();
    }

    fn finish_tool_call(&mut self, tool_call: ToolCall, result: ToolResult) {
        let id = tool_call.id.clone();
        let name = tool_call.function.name.clone();
        let ok = result.ok;
        let error = result.error.clone();
        let summary = result.summary.clone();
        self.finish_tool_step(&tool_call, &result);
        let tool_message = Message::tool_result(id.clone(), result.content);
        self.conversation.push(tool_message.clone());
        self.turn_messages.push(tool_message);

        match self.tools.execution_kind(&tool_call) {
            ToolExecutionKind::Standard => {
                self.pending.push_back(AgentEvent::ToolCallFinished {
                    id,
                    name,
                    ok,
                    summary,
                });
            }
            ToolExecutionKind::Subagent { task, agent_name } => {
                let mut summary =
                    summary
                        .and_then(|summary| summary.subagent)
                        .unwrap_or_else(|| SubagentExecutionSummary {
                            agent_name: Some(agent_name.clone()),
                            task,
                            result: None,
                            error: error
                                .or_else(|| (!ok).then(|| "subagent execution failed".to_string())),
                            model_calls: 0,
                            tool_calls: 0,
                            truncated: false,
                        });
                if summary.agent_name.is_none() {
                    summary.agent_name = Some(agent_name);
                }
                self.pending
                    .push_back(AgentEvent::SubagentFinished { id, ok, summary });
            }
        }
    }

    fn finish_tool_step(&mut self, tool_call: &ToolCall, result: &ToolResult) {
        if let Some(step) = self
            .turn
            .steps
            .iter_mut()
            .find(|step| step.tool_call_id.as_deref() == Some(tool_call.id.as_str()))
        {
            if result.ok {
                step.complete();
            } else {
                step.fail(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "tool call failed".to_string()),
                );
            }
        }
    }

    fn start_next_model_call(&mut self) {
        self.turn.steps.push(TurnStep::running_model_call());
        self.model_start = Some(self.model.stream(ModelRequest {
            conversation: self.conversation.clone(),
            tools: self.tool_definitions.clone(),
        }));
    }
}

impl Unpin for AgentTurnStream<'_> {}

impl Drop for AgentTurnStream<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // 调用方提前退出时也要通知工具。字段随后按正常 Drop 顺序释放，shell 的
            // 进程组 guard 等资源清理逻辑因此仍会执行。
            self.tool_context.cancellation.cancel();
        }
    }
}

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
            if !this.tool_futures.is_empty() {
                match Pin::new(&mut this.tool_futures).poll_next(cx) {
                    Poll::Ready(Some(outcome)) => {
                        if outcome.serial {
                            this.active_serial_tool = false;
                        }
                        this.finish_tool_execution(
                            outcome.index,
                            outcome.tool_call,
                            outcome.execution,
                        );
                        this.start_ready_tool_calls();
                        this.maybe_finish_tool_batch();
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    Poll::Ready(None) => {}
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
                match model_stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(ModelEvent::ReasoningDelta(text)))) => {
                        this.assistant_reasoning.push_str(&text);
                        return Poll::Ready(Some(AgentEvent::ReasoningDelta(text)));
                    }
                    Poll::Ready(Some(Ok(ModelEvent::TextDelta(text)))) => {
                        this.assistant_text.push_str(&text);
                        return Poll::Ready(Some(AgentEvent::TextDelta(text)));
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
                agent_name: "后藤一里".to_string(),
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
                .with_agent_name("后藤一里");
                ToolExecution::Completed(ToolResult {
                    ok: true,
                    content: serde_json::to_string(&json!({
                        "ok": true,
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
            events.push(event);
        }
        let turn = apply_record(thread, stream.into_turn_record());
        (events, turn)
    }

    async fn next_event(stream: &mut AgentTurnStream<'_>) -> AgentEvent {
        stream.next().await.expect("next agent event")
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
    fn agent_defaults_to_two_hundred_tool_rounds() {
        let model = ScriptedModel::new(Vec::new());
        let agent = Agent::new(&model, "system");

        assert_eq!(agent.max_tool_rounds, 200);
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
                AgentEvent::SubagentStarted {
                    id: start_id,
                    agent_name: Some(start_name),
                    task,
                },
                AgentEvent::SubagentFinished { id: finish_id, ok: true, summary },
                AgentEvent::TextDelta(_),
                AgentEvent::AgentMessage(_),
                AgentEvent::TurnCompleted,
            ] if start_id == "call-1"
                && finish_id == "call-1"
                && start_name == "后藤一里"
                && task == "Inspect runtime"
                && summary.agent_name.as_deref() == Some("后藤一里")
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
        let model = client(base_url);
        let tools = tools(&root);
        let agent = Agent::with_tools(&model, "You are helpful.", &tools);
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&thread, "Read note.txt")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream, &mut thread).await;

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
                AgentEvent::TextDelta(prefix),
                AgentEvent::ToolCallStarted { .. },
                AgentEvent::ToolCallFinished { ok: true, .. },
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
        let model = client(base_url);
        let tools = tools(&root);
        let agent = Agent::with_tools(&model, "You are helpful.", &tools);
        let thread = Thread::new();

        let mut stream = agent
            .run_turn(&thread, "Write note.txt")
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
}
