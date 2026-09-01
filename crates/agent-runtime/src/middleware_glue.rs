use super::*;

#[derive(Debug, Clone)]
pub struct MiddlewareCompactionOutcome {
    pub outcome: CompactionOutcome,
    pub events: Vec<AgentEvent>,
    pub additional_context: Vec<MiddlewareContextBlock>,
}

pub struct MiddlewareCompactionContext<'a> {
    pub client: &'a dyn Model,
    pub system_prompt: &'a str,
    pub context_config: ContextConfig,
    pub model_limits: ModelContextLimits,
    pub prompt: &'a str,
    pub tools: &'a [ToolDefinition],
    pub execution_context: MiddlewareExecutionContext,
    pub registry: &'a MiddlewareRegistry,
}

#[derive(Debug)]
pub struct MiddlewareCompactionError {
    pub error: RuntimeError,
    pub events: Vec<AgentEvent>,
}

impl std::fmt::Display for MiddlewareCompactionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for MiddlewareCompactionError {}

#[derive(Clone, Copy)]
pub struct MiddlewareAgentTurnContext<'a> {
    pub turn: RunAgentTurnContext<'a>,
    pub registry: &'a MiddlewareRegistry,
    pub agent_scope: MiddlewareAgentScope,
}

impl<'a> MiddlewareAgentTurnContext<'a> {
    pub fn new(
        turn: RunAgentTurnContext<'a>,
        registry: &'a MiddlewareRegistry,
        agent_scope: MiddlewareAgentScope,
    ) -> Self {
        Self {
            turn,
            registry,
            agent_scope,
        }
    }

    pub(crate) fn execution_context(
        self,
        cancellation: &CancellationToken,
        operation_id: Option<String>,
        turn_id: Option<String>,
    ) -> MiddlewareExecutionContext {
        middleware_execution_context(
            self.turn,
            cancellation,
            self.agent_scope,
            operation_id,
            turn_id,
        )
    }

    pub(crate) fn run_config(
        self,
        initial_context: Vec<MiddlewareContextBlock>,
        event_index: usize,
    ) -> MiddlewareRunConfig<'a> {
        MiddlewareRunConfig {
            registry: self.registry,
            agent_scope: self.agent_scope,
            initial_context,
            event_index,
        }
    }
}

pub(crate) struct MiddlewareRunConfig<'a> {
    pub(crate) registry: &'a MiddlewareRegistry,
    pub(crate) agent_scope: MiddlewareAgentScope,
    pub(crate) initial_context: Vec<MiddlewareContextBlock>,
    pub(crate) event_index: usize,
}

pub(crate) fn middleware_execution_context(
    context: RunAgentTurnContext<'_>,
    cancellation: &CancellationToken,
    agent_scope: MiddlewareAgentScope,
    operation_id: Option<String>,
    turn_id: Option<String>,
) -> MiddlewareExecutionContext {
    MiddlewareExecutionContext {
        invocation_id: None,
        session: context.session_name.to_string(),
        workspace_root: context.workspace_root.to_path_buf(),
        turn_index: context.turn_index,
        operation_id,
        turn_id,
        model: context.model.clone(),
        permissions: context.permissions,
        agent_scope,
        cancellation: cancellation.clone(),
    }
}

pub(crate) async fn deliver_middleware_events(
    context: RunAgentTurnContext<'_>,
    handler: &mut impl TurnEventHandler,
    handle: Option<&SessionHandle>,
    events: Vec<AgentEvent>,
    mut event_index: usize,
) -> Result<usize, RuntimeError> {
    for event in events {
        if let AgentEvent::MiddlewareFinished(invocation) = &event
            && let Some(handle) = handle
        {
            handle
                .commit_fact(
                    None,
                    None,
                    SessionFact::MiddlewareFinished {
                        invocation: invocation.clone(),
                    },
                )
                .await?;
        }
        let envelope = make_event_envelope(
            context.session_name,
            context.workspace_root,
            context.turn_index,
            event_index,
            event,
        );
        event_index += 1;
        handler.on_event(&envelope)?;
    }
    Ok(event_index)
}

pub(crate) async fn deliver_turn_middleware_events(
    context: RunAgentTurnContext<'_>,
    handler: &mut impl TurnEventHandler,
    fact_run: &mut Option<&mut SessionFactRun<'_>>,
    events: Vec<AgentEvent>,
    event_index: &mut usize,
) -> Result<(), RuntimeError> {
    for event in events {
        let envelope = make_event_envelope(
            context.session_name,
            context.workspace_root,
            context.turn_index,
            *event_index,
            event.clone(),
        );
        *event_index += 1;
        if let Some(run) = fact_run.as_deref_mut() {
            run.persist_event(&event).await?;
        }
        handler.on_event(&envelope)?;
    }
    Ok(())
}

pub(crate) fn render_middleware_context(
    blocks: &[MiddlewareContextBlock],
    heading: &str,
) -> String {
    let mut content = heading.to_string();
    for block in blocks {
        let _ = write!(
            content,
            "\n\n[{}/{}]\n{}",
            block.middleware_id,
            middleware_stage_name(block.stage),
            block.content
        );
    }
    content
}

fn middleware_stage_name(stage: agent_protocol::MiddlewareStage) -> &'static str {
    match stage {
        agent_protocol::MiddlewareStage::BeforePrompt => "before_prompt",
        agent_protocol::MiddlewareStage::BeforeTool => "before_tool",
        agent_protocol::MiddlewareStage::PermissionRequest => "permission_request",
        agent_protocol::MiddlewareStage::AfterTool => "after_tool",
        agent_protocol::MiddlewareStage::AfterTurn => "after_turn",
        agent_protocol::MiddlewareStage::PreCompact => "pre_compact",
        agent_protocol::MiddlewareStage::PostCompact => "post_compact",
    }
}

pub struct PreparedMiddlewareSessionTurn {
    pub operation_id: String,
    pub turn_id: String,
    /// 写入 `TurnStarted` fact 时拼好的 turn base prompt（含 AGENTS.md 与
    /// `<environment>` 块，不含 subagent guidance），运行阶段直接复用。
    pub system_prompt: String,
    pub initial_context: Vec<MiddlewareContextBlock>,
    pub event_index: usize,
}

impl PreparedMiddlewareSessionTurn {
    pub fn with_prompt(self, prompt: &str) -> PreparedMiddlewareTurn<'_> {
        PreparedMiddlewareTurn {
            turn: PreparedSessionTurn {
                operation_id: self.operation_id,
                turn_id: self.turn_id,
                prompt,
                system_prompt: self.system_prompt,
            },
            initial_context: self.initial_context,
            event_index: self.event_index,
        }
    }
}

pub struct PreparedMiddlewareTurn<'a> {
    pub turn: PreparedSessionTurn<'a>,
    pub initial_context: Vec<MiddlewareContextBlock>,
    pub event_index: usize,
}
