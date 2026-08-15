use agent_core::{ModelEvent, ToolExecutionMode};
use agent_protocol::{Role, Thread, ToolCall};
use serde_json::Value;

/// One scripted model response in a scenario.
///
/// `ToolCalls`, `Error`, `Truncate` and `Completed` terminate the model
/// response for the agent loop: after `ToolCalls` the core stops reading this
/// model stream and schedules the tools; after `Error` or `Completed` the
/// stream is done; `Truncate` models a provider stream that ends without any
/// completion marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStep {
    Reasoning(String),
    Text(String),
    ToolCalls(Vec<ToolCall>),
    Error(String),
    Truncate,
    Completed,
}

impl ModelStep {
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::Reasoning(text.into())
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        Self::ToolCalls(calls)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub fn truncate() -> Self {
        Self::Truncate
    }

    pub fn completed() -> Self {
        Self::Completed
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::ToolCalls(_) | Self::Error(_) | Self::Truncate | Self::Completed
        )
    }
}

/// A `Vec<ModelStep>` is one model response; a scenario's script is the
/// sequence of responses the fake model produces for model call 0, 1, 2, ...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelScript(pub Vec<ModelStep>);

impl ModelScript {
    pub fn new(steps: Vec<ModelStep>) -> Self {
        Self(steps)
    }
}

/// What the scripted tool does when the agent calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResponse {
    /// Return a successful tool result with this content.
    Ok(String),
    /// Return a failed tool result with this error message.
    Fail(String),
    /// First request approval; if approved return `approved`, if denied the
    /// core feeds an approval-denied tool error back to the model.
    Approval { approved: String, denied: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolBehavior {
    /// Always respond with this behavior.
    Always(ToolResponse),
    /// Respond with these behaviors in order; the last one repeats.
    Sequence(Vec<ToolResponse>),
}

impl ToolBehavior {
    pub fn ok(content: impl Into<String>) -> Self {
        Self::Always(ToolResponse::Ok(content.into()))
    }

    pub fn fail(error: impl Into<String>) -> Self {
        Self::Always(ToolResponse::Fail(error.into()))
    }

    pub fn sequence(responses: Vec<ToolResponse>) -> Self {
        Self::Sequence(responses)
    }
}

/// A scripted tool exposed to the agent for one scenario.
#[derive(Debug, Clone)]
pub struct ScenarioTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub mode: ToolExecutionMode,
    pub behavior: ToolBehavior,
}

impl ScenarioTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        mode: ToolExecutionMode,
        behavior: ToolBehavior,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            mode,
            behavior,
        }
    }

    pub fn with_parameters(mut self, parameters: Value) -> Self {
        self.parameters = parameters;
        self
    }
}

/// How the harness answers `ApprovalRequested` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    Approve,
    #[default]
    Deny,
}

/// Assert that a substring appears somewhere in the messages of a recorded
/// model request. This is how we prove tool output really reached the next
/// model call instead of being dropped or reordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestAssertion {
    /// Zero-based model call index.
    pub model_call_index: usize,
    pub contains: String,
}

impl RequestAssertion {
    pub fn new(model_call_index: usize, contains: impl Into<String>) -> Self {
        Self {
            model_call_index,
            contains: contains.into(),
        }
    }
}

/// Hard success criteria for a scenario. Every criterion is evaluated against
/// observable facts (turn record, events, recorded model/tool calls), never
/// against model judgment.
#[derive(Debug, Clone, Default)]
pub struct Expectations {
    /// Expected final turn status. Defaults to `Some(true)` (Completed).
    pub turn_completed: Option<bool>,
    /// Required substrings in the final assistant answer.
    pub final_text_contains: Vec<String>,
    /// Optional exact final assistant answer.
    pub final_text_equals: Option<String>,
    /// Optional exact reasoning content recorded on the final assistant message.
    pub assistant_reasoning_equals: Option<String>,
    /// Required substrings in the turn error (for failure scenarios).
    pub error_contains: Vec<String>,
    /// Exact ordered sequence of tool names the agent executed.
    pub tool_sequence: Option<Vec<String>>,
    /// Exact number of executed tool calls (events, not script length).
    pub tool_calls_started: Option<usize>,
    /// Exact number of tool calls that finished with `ok == false`.
    pub tool_calls_failed: Option<usize>,
    /// Exact number of model requests the loop made.
    pub model_calls: Option<usize>,
    /// Exact ordered role sequence in the persisted `TurnRecord.messages`.
    /// Pins the conversation chain: user -> assistant(tool_calls) -> tool -> assistant(final).
    pub message_roles: Option<Vec<Role>>,
    /// Substring assertions against recorded model request messages.
    pub model_request_contains: Vec<RequestAssertion>,
    /// Exact number of approval requests surfaced to the harness.
    pub approvals_requested: Option<usize>,
}

impl Expectations {
    pub fn completed() -> Self {
        Self {
            turn_completed: Some(true),
            ..Self::default()
        }
    }

    pub fn failed() -> Self {
        Self {
            turn_completed: Some(false),
            ..Self::default()
        }
    }

    pub fn contains(mut self, text: impl Into<String>) -> Self {
        self.final_text_contains.push(text.into());
        self
    }

    pub fn equals(mut self, text: impl Into<String>) -> Self {
        self.final_text_equals = Some(text.into());
        self
    }

    pub fn reasoning_equals(mut self, text: impl Into<String>) -> Self {
        self.assistant_reasoning_equals = Some(text.into());
        self
    }

    pub fn error_contains(mut self, text: impl Into<String>) -> Self {
        self.error_contains.push(text.into());
        self
    }

    pub fn tool_sequence(mut self, names: Vec<&str>) -> Self {
        self.tool_sequence = Some(names.into_iter().map(str::to_string).collect());
        self
    }

    pub fn tool_calls_started(mut self, count: usize) -> Self {
        self.tool_calls_started = Some(count);
        self
    }

    pub fn tool_calls_failed(mut self, count: usize) -> Self {
        self.tool_calls_failed = Some(count);
        self
    }

    pub fn model_calls(mut self, count: usize) -> Self {
        self.model_calls = Some(count);
        self
    }

    pub fn message_roles(mut self, roles: Vec<Role>) -> Self {
        self.message_roles = Some(roles);
        self
    }

    pub fn request_contains(mut self, model_call_index: usize, text: impl Into<String>) -> Self {
        self.model_request_contains
            .push(RequestAssertion::new(model_call_index, text));
        self
    }

    pub fn approvals_requested(mut self, count: usize) -> Self {
        self.approvals_requested = Some(count);
        self
    }
}

/// Upper bounds that a passing run may not exceed. They turn accidental
/// regressions (extra model rounds, extra tool calls, larger context) into CI
/// failures instead of silent cost increases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub max_model_calls: usize,
    pub max_tool_calls: usize,
    pub max_estimated_tokens: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_model_calls: usize::MAX,
            max_tool_calls: usize::MAX,
            max_estimated_tokens: usize::MAX,
        }
    }
}

impl Budget {
    pub fn new(max_model_calls: usize, max_tool_calls: usize, max_estimated_tokens: usize) -> Self {
        Self {
            max_model_calls,
            max_tool_calls,
            max_estimated_tokens,
        }
    }
}

/// A fully self-contained, deterministic scenario. One scenario = one agent
/// turn against a scripted model and scripted tools.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub description: String,
    pub system_prompt: String,
    pub thread: Thread,
    pub prompt: String,
    pub tools: Vec<ScenarioTool>,
    pub script: Vec<ModelScript>,
    pub max_tool_rounds: usize,
    pub approval_policy: ApprovalPolicy,
    pub expectations: Expectations,
    pub budget: Budget,
}

impl Scenario {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            system_prompt: String::from("You are a coding agent under evaluation."),
            thread: Thread::default(),
            prompt: prompt.into(),
            tools: Vec::new(),
            script: Vec::new(),
            max_tool_rounds: 99,
            approval_policy: ApprovalPolicy::Deny,
            expectations: Expectations::completed(),
            budget: Budget::default(),
        }
    }

    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    pub fn with_thread(mut self, thread: Thread) -> Self {
        self.thread = thread;
        self
    }

    pub fn with_tool(mut self, tool: ScenarioTool) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_script(mut self, script: ModelScript) -> Self {
        self.script.push(script);
        self
    }

    pub fn with_max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = rounds.max(1);
        self
    }

    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval_policy = policy;
        self
    }

    pub fn with_expectations(mut self, expectations: Expectations) -> Self {
        self.expectations = expectations;
        self
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Structural validation of the scenario definition itself, before any
    /// model or tool runs. A bad scenario must be a compile/CI error, not a
    /// mysterious model behavior.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.id.trim().is_empty() {
            errors.push("scenario id must not be empty".to_string());
        }
        if self.description.trim().is_empty() {
            errors.push(format!(
                "scenario {}: description must not be empty",
                self.id
            ));
        }
        if self.script.is_empty() {
            errors.push(format!("scenario {}: model script is empty", self.id));
        }
        if self.budget.max_model_calls < self.script.len() {
            errors.push(format!(
                "scenario {}: budget.max_model_calls ({}) is below script length ({})",
                self.id,
                self.budget.max_model_calls,
                self.script.len()
            ));
        }

        let mut seen_tools = std::collections::HashSet::new();
        for tool in &self.tools {
            if tool.name.trim().is_empty() {
                errors.push(format!("scenario {}: tool name must not be empty", self.id));
            }
            if !seen_tools.insert(tool.name.as_str()) {
                errors.push(format!(
                    "scenario {}: duplicate tool name {:?}",
                    self.id, tool.name
                ));
            }
            match &tool.behavior {
                ToolBehavior::Always(_) => {}
                ToolBehavior::Sequence(responses) if responses.is_empty() => {
                    errors.push(format!(
                        "scenario {}: tool {:?} has an empty response sequence",
                        self.id, tool.name
                    ));
                }
                ToolBehavior::Sequence(_) => {}
            }
        }

        for (call_index, script) in self.script.iter().enumerate() {
            if script.0.is_empty() {
                errors.push(format!(
                    "scenario {}: model response {} is empty",
                    self.id, call_index
                ));
                continue;
            }
            let last = script.0.last().expect("response is non-empty");
            if !last.is_terminal() {
                errors.push(format!(
                    "scenario {}: model response {} must end with ToolCalls, Error, Truncate or Completed, got {:?}",
                    self.id, call_index, last
                ));
            }
            if script.0[..script.0.len() - 1]
                .iter()
                .any(ModelStep::is_terminal)
            {
                errors.push(format!(
                    "scenario {}: model response {} has a terminal step before the last step",
                    self.id, call_index
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Convert the scenario script into concrete model events. Called by the
    /// harness after validation.
    pub fn build_model_events(&self) -> Vec<Vec<Result<ModelEvent, String>>> {
        self.script
            .iter()
            .map(|script| {
                script
                    .0
                    .iter()
                    .filter_map(|step| match step {
                        ModelStep::Reasoning(text) => {
                            Some(Ok(ModelEvent::ReasoningDelta(text.clone())))
                        }
                        ModelStep::Text(text) => Some(Ok(ModelEvent::TextDelta(text.clone()))),
                        ModelStep::ToolCalls(calls) => {
                            Some(Ok(ModelEvent::ToolCalls(calls.clone())))
                        }
                        ModelStep::Error(message) => Some(Err(message.clone())),
                        ModelStep::Truncate => None,
                        ModelStep::Completed => Some(Ok(ModelEvent::Completed)),
                    })
                    .collect()
            })
            .collect()
    }
}
