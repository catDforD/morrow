use super::*;

const DEFAULT_MAX_TOOL_ROUNDS: usize = 99;
const MAX_CONCURRENT_TOOL_CALLS: usize = 4;
/// after_turn middleware 打回继续的次数上限；超限强制完成，避免验收钩子把 turn 卡成死循环。
pub(crate) const MAX_AFTER_TURN_CONTINUES: usize = 3;
/// turn 内护栏触发后注入的收尾指令：要求模型停止使用工具并总结进展。
const CONTEXT_LIMIT_WRAP_UP_PROMPT: &str = "The conversation has reached its context token limit for this turn and cannot grow further. Do not call any more tools. Using only the information already gathered, summarize your progress so far, the partial results you have, and any remaining blockers or unfinished work, then stop.";
const CONTEXT_LIMIT_FAILURE: &str = "context limit exceeded mid-turn";

#[derive(Debug, Clone, Default)]
pub struct AgentRunContext {
    pub tool: ToolExecutionContext,
    pub middleware: Option<MiddlewareExecutionContext>,
    pub initial_context: Vec<MiddlewareContextBlock>,
    /// turn 内上下文护栏的绝对 token 上限。每次发起后续模型调用前估算当前
    /// conversation；超限则进入一次性收尾模式（无工具的总结调用）。
    /// `None` 关闭护栏。
    pub context_token_limit: Option<usize>,
}

#[derive(Clone)]
pub struct Agent<'a> {
    model: &'a dyn Model,
    system_prompt: String,
    tools: &'a dyn ToolRuntime,
    middleware: AgentMiddlewareChain,
    pub(crate) max_tool_rounds: usize,
}

impl fmt::Debug for Agent<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("system_prompt", &self.system_prompt)
            .field("tool_count", &self.tools.definitions().len())
            .field("middleware", &self.middleware)
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
            middleware: AgentMiddlewareChain::default(),
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
            middleware: AgentMiddlewareChain::default(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }

    pub fn with_middleware(mut self, middleware: AgentMiddlewareChain) -> Self {
        self.middleware = middleware;
        self
    }

    pub fn with_max_tool_rounds(mut self, max_tool_rounds: usize) -> Self {
        self.max_tool_rounds = max_tool_rounds.max(1);
        self
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
        self.run_turn_with_agent_context(
            thread,
            prompt,
            AgentRunContext {
                tool: tool_context,
                ..AgentRunContext::default()
            },
        )
        .await
    }

    pub async fn run_turn_with_agent_context<'b>(
        &'b self,
        thread: &Thread,
        prompt: impl Into<String>,
        run_context: AgentRunContext,
    ) -> Result<AgentTurnStream<'b>, AgentError> {
        let user_message = Message::user(prompt.into());
        let mut conversation = Conversation::with_system_prompt(self.system_prompt.clone());
        conversation.messages.extend(thread.messages.clone());
        append_middleware_context_message(&mut conversation, &run_context.initial_context);
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
            tool_context: run_context.tool,
            middleware: self.middleware.clone(),
            middleware_context: run_context.middleware,
            tool_definitions,
            max_tool_rounds: self.max_tool_rounds,
            context_token_limit: run_context.context_token_limit,
            wrap_up_started: false,
            conversation,
            model_stream: None,
            model_start: Some(model_start),
            pending_tool_calls: VecDeque::new(),
            tool_futures: FuturesUnordered::new(),
            pending_tool_results: BTreeMap::new(),
            pending_middleware_context: BTreeMap::new(),
            next_tool_result_index: 0,
            active_serial_tool: false,
            processing_tool_calls: false,
            pending_approval: None,
            pending_after_turn: None,
            after_turn_continues: 0,
            turn: Turn::running(user_message.clone()),
            turn_messages: vec![user_message.clone()],
            assistant_reasoning: String::new(),
            assistant_text: String::new(),
            model_call_index: 0,
            pending: VecDeque::from([AgentEvent::TurnStarted, AgentEvent::ModelCallStarted]),
            finished: false,
            tool_rounds: 0,
        })
    }
}

type ModelStartFuture = ModelFuture;
type ToolCallFuture = BoxFuture<'static, ToolCallOutcome>;
type AfterTurnFuture = BoxFuture<'static, AfterTurnRun>;

#[derive(Debug, Clone)]
struct PendingApproval {
    index: usize,
    tool_call: ToolCall,
    request: ApprovalRequest,
    serial: bool,
}

pub struct AgentTurnStream<'a> {
    model: &'a dyn Model,
    tools: &'a dyn ToolRuntime,
    tool_context: ToolExecutionContext,
    middleware: AgentMiddlewareChain,
    middleware_context: Option<MiddlewareExecutionContext>,
    tool_definitions: Vec<ToolDefinition>,
    max_tool_rounds: usize,
    context_token_limit: Option<usize>,
    wrap_up_started: bool,
    conversation: Conversation,
    model_stream: Option<ModelStream>,
    model_start: Option<ModelStartFuture>,
    pending_tool_calls: VecDeque<(usize, ToolCall)>,
    tool_futures: FuturesUnordered<ToolCallFuture>,
    pending_tool_results: BTreeMap<usize, (ToolCall, ToolExecution)>,
    pending_middleware_context: BTreeMap<usize, Vec<MiddlewareContextBlock>>,
    next_tool_result_index: usize,
    active_serial_tool: bool,
    processing_tool_calls: bool,
    pending_approval: Option<PendingApproval>,
    pending_after_turn: Option<AfterTurnFuture>,
    after_turn_continues: usize,
    turn: Turn,
    turn_messages: Vec<Message>,
    assistant_reasoning: String,
    assistant_text: String,
    model_call_index: usize,
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
        self.pending_after_turn = None;
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
            self.active_serial_tool = true;
            self.start_tool_execution(
                pending_approval.index,
                pending_approval.tool_call,
                pending_approval.serial,
                Some(ToolApproval {
                    decision,
                    request: pending_approval.request,
                }),
            );
        } else {
            let result = ToolResult::error("approval denied");
            self.active_serial_tool = false;
            self.finish_tool_execution(
                pending_approval.index,
                pending_approval.tool_call,
                ToolExecution::Completed(result),
            );
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
        self.pending.push_back(AgentEvent::ModelMessageCommitted {
            model_call_id: format!("model-call-{}", self.model_call_index),
            message: self
                .turn
                .assistant_message
                .clone()
                .expect("completed turn has assistant message"),
        });
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

    /// 模型流 Completed 后先过 after_turn middleware 链，再决定完成、打回还是判负。
    /// 无 middleware 上下文时保持原路径直接完成。
    fn start_after_turn_middleware(&mut self) {
        let Some(context) = self.middleware_context.clone() else {
            self.complete_turn();
            return;
        };
        let middleware = self.middleware.clone();
        let input = AfterTurnInput {
            context,
            final_text: self.assistant_text.clone(),
            tool_call_count: self
                .turn
                .steps
                .iter()
                .filter(|step| step.tool_call_id.is_some())
                .count(),
            turn_message_count: self.turn_messages.len(),
            tool_names: self
                .turn
                .steps
                .iter()
                .filter_map(|step| step.tool_name.clone())
                .collect(),
        };
        self.pending_after_turn =
            Some(async move { middleware.run_after_turn(input).await }.boxed());
    }

    fn handle_after_turn_run(&mut self, run: AfterTurnRun) {
        self.pending.extend(run.events);
        if run.cancelled {
            return;
        }
        if !run.fail_reasons.is_empty() {
            self.fail_turn(format!(
                "after-turn middleware rejected completion: {}",
                run.fail_reasons.join("; ")
            ));
            return;
        }
        if !run.continue_requested {
            self.complete_turn();
            return;
        }
        if self.after_turn_continues >= MAX_AFTER_TURN_CONTINUES {
            self.pending.push_back(AgentEvent::Warning(format!(
                "after-turn middleware continuation limit ({MAX_AFTER_TURN_CONTINUES}) reached; completing the turn"
            )));
            self.complete_turn();
            return;
        }
        self.after_turn_continues += 1;
        // 与 complete_turn 前半相同：提交当前 assistant message，但 turn 继续运行。
        if let Some(step) = self.turn.steps.last_mut() {
            step.complete();
        }
        let assistant_message = Message::assistant(std::mem::take(&mut self.assistant_text))
            .with_reasoning_content(std::mem::take(&mut self.assistant_reasoning));
        self.conversation.push(assistant_message.clone());
        self.turn_messages.push(assistant_message.clone());
        self.pending.push_back(AgentEvent::ModelMessageCommitted {
            model_call_id: format!("model-call-{}", self.model_call_index),
            message: assistant_message,
        });
        append_middleware_context_message(&mut self.conversation, &run.context);
        self.start_next_model_call();
    }

    fn handle_tool_calls(&mut self, tool_calls: Vec<ToolCall>) {
        if self.wrap_up_started {
            // 收尾调用只有无工具的总结一次机会；仍请求工具则直接失败。
            self.fail_turn(CONTEXT_LIMIT_FAILURE);
            return;
        }
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
        self.turn_messages.push(assistant_message.clone());
        self.pending.push_back(AgentEvent::ModelMessageCommitted {
            model_call_id: format!("model-call-{}", self.model_call_index),
            message: assistant_message,
        });
        self.pending_tool_calls = tool_calls.into_iter().enumerate().collect();
        self.pending_tool_results.clear();
        self.pending_middleware_context.clear();
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
            ToolExecutionKind::Subagent { task, identity } => {
                self.pending.push_back(AgentEvent::SubagentStarted {
                    id: id.clone(),
                    agent_id: Some(identity.id),
                    agent_name: Some(identity.name),
                    task,
                });
            }
        }

        if serial {
            self.active_serial_tool = true;
        }
        let Some(context) = self.middleware_context.clone() else {
            self.start_tool_execution(index, tool_call, serial, None);
            return;
        };
        let middleware = self.middleware.clone();
        let call_for_result = tool_call.clone();
        self.tool_futures.push(
            async move {
                let run = middleware
                    .run_before_tool(BeforeToolInput {
                        context,
                        tool_call: call_for_result.clone(),
                    })
                    .await;
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    phase: ToolCallPhase::Before(run),
                    serial,
                }
            }
            .boxed(),
        );
    }

    fn start_tool_execution(
        &mut self,
        index: usize,
        tool_call: ToolCall,
        serial: bool,
        approval: Option<ToolApproval>,
    ) {
        let call_for_result = tool_call.clone();
        let execution = self.tools.execute(
            call_for_result.clone(),
            approval.clone(),
            self.tool_context.clone(),
        );
        self.tool_futures.push(
            async move {
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    phase: ToolCallPhase::Execution {
                        execution: execution.await,
                        approval_attempted: approval.is_some(),
                    },
                    serial,
                }
            }
            .boxed(),
        );
    }

    fn start_permission_middleware(
        &mut self,
        index: usize,
        tool_call: ToolCall,
        request: ApprovalRequest,
        serial: bool,
    ) {
        let Some(context) = self.middleware_context.clone() else {
            self.pending_approval = Some(PendingApproval {
                index,
                tool_call,
                request: request.clone(),
                serial,
            });
            self.pending
                .push_back(AgentEvent::ApprovalRequested(request));
            return;
        };
        let middleware = self.middleware.clone();
        let call_for_result = tool_call.clone();
        let request_for_result = request.clone();
        self.tool_futures.push(
            async move {
                let run = middleware
                    .run_permission_request(PermissionRequestInput {
                        context,
                        tool_call: call_for_result.clone(),
                        request: request_for_result,
                    })
                    .await;
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    phase: ToolCallPhase::Permission(run),
                    serial,
                }
            }
            .boxed(),
        );
        self.pending_tool_results
            .insert(index, (tool_call, ToolExecution::ApprovalRequired(request)));
    }

    fn start_after_tool_middleware(
        &mut self,
        index: usize,
        tool_call: ToolCall,
        result: ToolResult,
        serial: bool,
    ) {
        let Some(context) = self.middleware_context.clone() else {
            self.active_serial_tool &= !serial;
            self.finish_tool_execution(index, tool_call, ToolExecution::Completed(result));
            return;
        };
        let middleware = self.middleware.clone();
        let call_for_result = tool_call.clone();
        let result_for_input = result.clone();
        self.tool_futures.push(
            async move {
                let run = middleware
                    .run_after_tool(AfterToolInput {
                        context,
                        tool_call: call_for_result.clone(),
                        result: result_for_input,
                    })
                    .await;
                ToolCallOutcome {
                    index,
                    tool_call: call_for_result,
                    phase: ToolCallPhase::After {
                        result,
                        middleware: run,
                    },
                    serial,
                }
            }
            .boxed(),
        );
    }

    fn handle_tool_outcome(&mut self, outcome: ToolCallOutcome) {
        let ToolCallOutcome {
            index,
            tool_call,
            phase,
            serial,
        } = outcome;
        match phase {
            ToolCallPhase::Before(run) => {
                let denied = run.denied();
                let cancelled = run.cancelled;
                let denied_reasons = run.denied_reasons.clone();
                self.record_middleware_run(index, run.events, run.context);
                if cancelled {
                    self.active_serial_tool &= !serial;
                    return;
                }
                if denied {
                    self.active_serial_tool &= !serial;
                    self.finish_tool_execution(
                        index,
                        tool_call,
                        ToolExecution::Completed(ToolResult::error(format!(
                            "blocked by middleware: {}",
                            denied_reasons.join("; ")
                        ))),
                    );
                } else {
                    self.start_tool_execution(index, tool_call, serial, None);
                }
            }
            ToolCallPhase::Execution {
                execution,
                approval_attempted,
            } => match execution {
                ToolExecution::Completed(result) => {
                    self.start_after_tool_middleware(index, tool_call, result, serial);
                }
                ToolExecution::ApprovalRequired(_request) if approval_attempted => {
                    self.active_serial_tool &= !serial;
                    self.finish_tool_execution(
                        index,
                        tool_call,
                        ToolExecution::Completed(ToolResult::error(
                            "tool requested approval again after an approval decision",
                        )),
                    );
                }
                ToolExecution::ApprovalRequired(request) => {
                    self.start_permission_middleware(index, tool_call, request, serial);
                }
            },
            ToolCallPhase::Permission(run) => {
                let request = self
                    .pending_tool_results
                    .remove(&index)
                    .and_then(|(_, execution)| match execution {
                        ToolExecution::ApprovalRequired(request) => Some(request),
                        ToolExecution::Completed(_) => None,
                    })
                    .expect("permission middleware must retain its approval request");
                let decision = run.decision();
                let cancelled = run.cancelled;
                self.record_middleware_run(index, run.events, run.context);
                if cancelled {
                    self.active_serial_tool &= !serial;
                    return;
                }
                match decision {
                    PermissionDecision::Deny { reason } => {
                        self.active_serial_tool &= !serial;
                        self.finish_tool_execution(
                            index,
                            tool_call,
                            ToolExecution::Completed(ToolResult::error(format!(
                                "blocked by middleware: {reason}"
                            ))),
                        );
                    }
                    PermissionDecision::Approve { .. } => {
                        self.active_serial_tool = true;
                        self.start_tool_execution(
                            index,
                            tool_call,
                            serial,
                            Some(ToolApproval {
                                decision: ApprovalDecision::approve(request.id.clone()),
                                request,
                            }),
                        );
                    }
                    PermissionDecision::Continue => {
                        self.pending_approval = Some(PendingApproval {
                            index,
                            tool_call,
                            request: request.clone(),
                            serial,
                        });
                        self.pending
                            .push_back(AgentEvent::ApprovalRequested(request));
                    }
                }
            }
            ToolCallPhase::After { result, middleware } => {
                let cancelled = middleware.cancelled;
                let fatal_errors = middleware.fatal_errors.clone();
                self.record_middleware_run(index, middleware.events, middleware.context);
                self.active_serial_tool &= !serial;
                if cancelled {
                    return;
                }
                if !fatal_errors.is_empty() {
                    self.fail_turn(format!(
                        "after-tool middleware failed: {}",
                        fatal_errors.join("; ")
                    ));
                    return;
                }
                self.finish_tool_execution(index, tool_call, ToolExecution::Completed(result));
            }
        }
    }

    fn record_middleware_run(
        &mut self,
        index: usize,
        events: Vec<AgentEvent>,
        context: Vec<MiddlewareContextBlock>,
    ) {
        self.pending.extend(events);
        self.pending_middleware_context
            .entry(index)
            .or_default()
            .extend(context);
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
                        serial: true,
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
            let context = std::mem::take(&mut self.pending_middleware_context)
                .into_values()
                .flatten()
                .collect::<Vec<_>>();
            append_middleware_context_message(&mut self.conversation, &context);
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
        let tool_message = Message::tool_result(id.clone(), result.content);
        self.conversation.push(tool_message.clone());
        self.turn_messages.push(tool_message.clone());
        self.pending.push_back(AgentEvent::ToolResultCommitted {
            tool_call_id: id.clone(),
            message: tool_message,
            ok,
            summary: summary.clone(),
        });

        match self.tools.execution_kind(&tool_call) {
            ToolExecutionKind::Standard => {
                self.pending.push_back(AgentEvent::ToolCallFinished {
                    id,
                    name,
                    ok,
                    summary,
                });
            }
            ToolExecutionKind::Subagent { task, identity } => {
                let mut summary = summary
                    .and_then(|summary| summary.subagent.map(|subagent| *subagent))
                    .unwrap_or_else(|| SubagentExecutionSummary {
                        agent_id: Some(identity.id.clone()),
                        agent_name: Some(identity.name.clone()),
                        task,
                        result: None,
                        error: error
                            .or_else(|| (!ok).then(|| "subagent execution failed".to_string())),
                        model_calls: 0,
                        tool_calls: 0,
                        truncated: false,
                    });
                summary.agent_id.get_or_insert(identity.id);
                summary.agent_name.get_or_insert(identity.name);
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
        // turn 内护栏：发起后续模型调用前估算水位，超限则进入一次性收尾模式。
        if let Some(limit) = self.context_token_limit
            && !self.wrap_up_started
            && tokens::estimate_model_request_tokens(&self.conversation, &self.tool_definitions)
                > limit
        {
            self.wrap_up_started = true;
            // 收尾指令只注入本次模型调用的 conversation，不写入 turn 记录，
            // 避免把一次性的停止指令带进后续 turn 的历史。
            self.conversation
                .push(Message::system(CONTEXT_LIMIT_WRAP_UP_PROMPT));
        }
        let tools = if self.wrap_up_started {
            // 收尾调用不提供工具，强制模型直接总结。
            Vec::new()
        } else {
            self.tool_definitions.clone()
        };
        self.turn.steps.push(TurnStep::running_model_call());
        self.model_call_index += 1;
        self.pending.push_back(AgentEvent::ModelCallStarted);
        self.model_start = Some(self.model.stream(ModelRequest {
            conversation: self.conversation.clone(),
            tools,
        }));
    }

    /// 模型调用失败；若处于护栏收尾模式，统一以上下文超限作为失败原因。
    fn fail_model_call(&mut self, error: impl ToString) {
        if self.wrap_up_started {
            self.fail_turn(CONTEXT_LIMIT_FAILURE);
        } else {
            self.fail_turn(error);
        }
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
                        this.handle_tool_outcome(outcome);
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

            if let Some(future) = this.pending_after_turn.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(run) => {
                        this.pending_after_turn = None;
                        this.handle_after_turn_run(run);
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
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
                        this.fail_model_call(err);
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
                        this.start_after_turn_middleware();
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        this.model_stream = None;
                        this.fail_model_call(err);
                        return Poll::Ready(this.pending.pop_front());
                    }
                    Poll::Ready(None) => {
                        this.model_stream = None;
                        this.fail_model_call("model stream ended before completion");
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

fn append_middleware_context_message(
    conversation: &mut Conversation,
    blocks: &[MiddlewareContextBlock],
) {
    if blocks.is_empty() {
        return;
    }
    let mut content = String::from(
        "Additional middleware context for this operation. Each block is attributed to its source and is not part of the user's message or a tool result.",
    );
    for block in blocks {
        content.push_str("\n\n[");
        content.push_str(&block.middleware_id);
        content.push('/');
        content.push_str(match block.stage {
            agent_protocol::MiddlewareStage::BeforePrompt => "before_prompt",
            agent_protocol::MiddlewareStage::BeforeTool => "before_tool",
            agent_protocol::MiddlewareStage::PermissionRequest => "permission_request",
            agent_protocol::MiddlewareStage::AfterTool => "after_tool",
            agent_protocol::MiddlewareStage::AfterTurn => "after_turn",
            agent_protocol::MiddlewareStage::PreCompact => "pre_compact",
            agent_protocol::MiddlewareStage::PostCompact => "post_compact",
        });
        content.push_str("]\n");
        content.push_str(&block.content);
    }
    conversation.push(Message::system(content));
}
