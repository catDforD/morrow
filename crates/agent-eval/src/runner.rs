use crate::model::{RecordedModelRequest, ScriptedModel};
use crate::report::{Baseline, EVAL_REPORT_SCHEMA_VERSION, ScenarioMetrics, SuiteReport};
use crate::scenario::{ApprovalPolicy, Scenario};
use crate::tools::ScenarioToolRuntime;
use agent_core::{Agent, AgentRunContext};
use agent_protocol::{AgentEvent, ApprovalDecision, Message, TurnStatus};
use futures_util::StreamExt;
use std::time::Instant;

/// Timestamp provider kept behind a function so tests can pin reports.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Run every scenario and attach baseline comparisons. Invalid scenario
/// definitions fail loudly as scenario failures instead of being skipped.
pub async fn run_suite(scenarios: &[Scenario], baseline: Option<&Baseline>) -> SuiteReport {
    let generated_at_ms = now_ms();
    let mut results = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        let mut metrics = match scenario.validate() {
            Ok(()) => run_scenario(scenario).await,
            Err(errors) => invalid_scenario_metrics(scenario, errors),
        };
        if let Some(baseline) = baseline {
            metrics.baseline_failures = baseline.check(&scenario.id, &metrics);
        }
        let passed = metrics.all_failures().next().is_none();
        metrics.passed = passed;
        results.push(metrics);
    }

    let passed = results.iter().filter(|metrics| metrics.passed).count();
    let failed = results.len() - passed;
    SuiteReport {
        schema_version: EVAL_REPORT_SCHEMA_VERSION,
        generated_at_ms,
        scenario_count: results.len(),
        passed,
        failed,
        results,
    }
}

fn invalid_scenario_metrics(scenario: &Scenario, errors: Vec<String>) -> ScenarioMetrics {
    ScenarioMetrics {
        scenario_id: scenario.id.clone(),
        passed: false,
        turn_status: "invalid".to_string(),
        final_text: String::new(),
        turn_error: None,
        model_calls: 0,
        tool_calls_started: 0,
        tool_calls_ok: 0,
        tool_calls_failed: 0,
        approvals_requested: 0,
        approvals_resolved: 0,
        estimated_tokens: 0,
        duration_ms: 0,
        assertion_failures: errors
            .into_iter()
            .map(|error| format!("invalid scenario: {error}"))
            .collect(),
        budget_failures: Vec::new(),
        baseline_failures: Vec::new(),
    }
}

/// Run one scenario against the real `agent-core` loop with a scripted model
/// and scripted tools. Every observable fact is recorded for assertions.
pub async fn run_scenario(scenario: &Scenario) -> ScenarioMetrics {
    let started = Instant::now();
    let model = ScriptedModel::from_scenario(scenario.build_model_events());
    let requests = model.record_requests();
    let tool_runtime = ScenarioToolRuntime::new(&scenario.tools);
    let recorded_calls = tool_runtime.record_calls();

    let agent = Agent::with_tools(&model, scenario.system_prompt.clone(), &tool_runtime)
        .with_max_tool_rounds(scenario.max_tool_rounds);

    let mut stream = match agent
        .run_turn_with_agent_context(
            &scenario.thread,
            scenario.prompt.clone(),
            AgentRunContext {
                context_token_limit: scenario.context_token_limit,
                ..AgentRunContext::default()
            },
        )
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            return ScenarioMetrics {
                scenario_id: scenario.id.clone(),
                passed: false,
                turn_status: "failed".to_string(),
                final_text: String::new(),
                turn_error: Some(error.to_string()),
                model_calls: 0,
                tool_calls_started: 0,
                tool_calls_ok: 0,
                tool_calls_failed: 0,
                approvals_requested: 0,
                approvals_resolved: 0,
                estimated_tokens: 0,
                duration_ms: started.elapsed().as_millis() as u64,
                assertion_failures: vec![format!("failed to start agent turn: {error}")],
                budget_failures: Vec::new(),
                baseline_failures: Vec::new(),
            };
        }
    };

    let mut model_calls = 0usize;
    let mut tool_calls_started = 0usize;
    let mut tool_calls_ok = 0usize;
    let mut tool_calls_failed = 0usize;
    let mut approvals_requested = 0usize;
    let mut approvals_resolved = 0usize;
    let mut tool_sequence = Vec::new();
    let mut final_agent_message: Option<String> = None;
    let mut streamed_text = String::new();
    let mut output_chars = 0usize;
    let mut event_error: Option<String> = None;
    let mut assertion_failures = Vec::new();

    while let Some(event) = stream.next().await {
        let approval_request = match &event {
            AgentEvent::ApprovalRequested(request) => Some(request.clone()),
            _ => None,
        };

        match event {
            AgentEvent::TurnStarted => {}
            AgentEvent::ModelCallStarted => model_calls += 1,
            AgentEvent::ReasoningDelta(text) => {
                output_chars += text.chars().count();
            }
            AgentEvent::TextDelta(text) => {
                output_chars += text.chars().count();
                streamed_text.push_str(&text);
            }
            AgentEvent::AgentMessage(text) => final_agent_message = Some(text),
            AgentEvent::ToolCallStarted { name, .. } => {
                tool_calls_started += 1;
                output_chars += name.chars().count();
            }
            AgentEvent::ToolCallFinished { name, ok, .. } => {
                tool_sequence.push(name);
                if ok {
                    tool_calls_ok += 1;
                } else {
                    tool_calls_failed += 1;
                }
            }
            AgentEvent::ApprovalRequested(_) => approvals_requested += 1,
            AgentEvent::ApprovalResolved(_) => {}
            AgentEvent::Error(error) => event_error = Some(error),
            AgentEvent::ModelMessageCommitted { .. }
            | AgentEvent::ToolResultCommitted { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::SubagentUpdated(_)
            | AgentEvent::TurnCompleted
            | AgentEvent::Warning(_)
            | AgentEvent::MiddlewareStarted(_)
            | AgentEvent::MiddlewareFinished(_) => {}
        }

        if let Some(request) = approval_request {
            approvals_resolved += 1;
            let decision = match scenario.approval_policy {
                ApprovalPolicy::Approve => ApprovalDecision::approve(request.id.clone()),
                ApprovalPolicy::Deny => ApprovalDecision::deny(request.id.clone()),
            };
            if let Err(error) = stream.resolve_approval(decision) {
                assertion_failures.push(format!(
                    "harness failed to resolve approval {}: {error}",
                    request.id
                ));
                break;
            }
        }
    }

    let record = stream.into_turn_record();
    let turn = &record.turn;
    let turn_status = match turn.status {
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Running => "running",
    };
    let final_text = final_agent_message.unwrap_or(streamed_text);
    let reasoning = turn
        .assistant_message
        .as_ref()
        .and_then(|message| message.reasoning_content.as_deref())
        .unwrap_or_default()
        .to_string();
    let turn_error = turn.error.clone().or(event_error);

    let (input_tokens, request_snapshots) = snapshot_requests(&requests);
    let estimated_tokens = input_tokens + output_chars.div_ceil(4);

    evaluate_expectations(
        scenario,
        turn_status,
        &final_text,
        &reasoning,
        turn_error.as_deref(),
        &tool_sequence,
        &record.messages,
        &request_snapshots,
        model_calls,
        tool_calls_started,
        tool_calls_failed,
        approvals_requested,
        &mut assertion_failures,
    );

    let budget_failures = evaluate_budget(
        &scenario.budget,
        model_calls,
        tool_calls_started,
        estimated_tokens,
    );

    // Tool calls are recorded by the runtime regardless of event delivery.
    // Approval flows legitimately execute the tool twice (request + granted
    // retry), so the invariant is a range, not equality.
    let recorded_call_count = recorded_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    if !(tool_calls_started..=tool_calls_started + approvals_requested)
        .contains(&recorded_call_count)
    {
        assertion_failures.push(format!(
            "event/tool divergence: {recorded_call_count} calls executed for {tool_calls_started} started calls and {approvals_requested} approvals"
        ));
    }

    let passed = assertion_failures.is_empty() && budget_failures.is_empty();
    ScenarioMetrics {
        scenario_id: scenario.id.clone(),
        passed,
        turn_status: turn_status.to_string(),
        final_text,
        turn_error,
        model_calls,
        tool_calls_started,
        tool_calls_ok,
        tool_calls_failed,
        approvals_requested,
        approvals_resolved,
        estimated_tokens,
        duration_ms: started.elapsed().as_millis() as u64,
        assertion_failures,
        budget_failures,
        baseline_failures: Vec::new(),
    }
}

fn snapshot_requests(
    requests: &std::sync::Arc<std::sync::Mutex<Vec<RecordedModelRequest>>>,
) -> (usize, Vec<RecordedModelRequest>) {
    let snapshots = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let tokens = snapshots.iter().map(estimate_request_tokens).sum();
    (tokens, snapshots)
}

fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Deterministic token proxy. Roughly mirrors the runtime's accounting:
/// per-message base overhead + `chars / 4` + serialized tool definitions.
fn estimate_request_tokens(request: &RecordedModelRequest) -> usize {
    let mut total = 8usize; // request envelope overhead
    for message in &request.messages {
        total += 4;
        if let Some(content) = message.content.as_deref() {
            total += estimate_text_tokens(content);
        }
        if let Some(reasoning) = message.reasoning_content.as_deref() {
            total += estimate_text_tokens(reasoning);
        }
        if let Some(tool_calls) = message.tool_calls.as_deref() {
            total += serde_json::to_string(tool_calls)
                .map(|value| estimate_text_tokens(&value))
                .unwrap_or(0);
        }
    }
    if !request.tool_definitions.is_empty() {
        total += serde_json::to_string(&request.tool_definitions)
            .map(|value| estimate_text_tokens(&value) + 8)
            .unwrap_or(0);
    }
    total
}

fn evaluate_budget(
    budget: &crate::scenario::Budget,
    model_calls: usize,
    tool_calls: usize,
    estimated_tokens: usize,
) -> Vec<String> {
    let mut failures = Vec::new();
    if model_calls > budget.max_model_calls {
        failures.push(format!(
            "budget exceeded: {model_calls} model calls (limit {})",
            budget.max_model_calls
        ));
    }
    if tool_calls > budget.max_tool_calls {
        failures.push(format!(
            "budget exceeded: {tool_calls} tool calls (limit {})",
            budget.max_tool_calls
        ));
    }
    if estimated_tokens > budget.max_estimated_tokens {
        failures.push(format!(
            "budget exceeded: {estimated_tokens} estimated tokens (limit {})",
            budget.max_estimated_tokens
        ));
    }
    failures
}

#[allow(clippy::too_many_arguments)]
fn evaluate_expectations(
    scenario: &Scenario,
    turn_status: &str,
    final_text: &str,
    reasoning: &str,
    turn_error: Option<&str>,
    tool_sequence: &[String],
    messages: &[Message],
    requests: &[RecordedModelRequest],
    model_calls: usize,
    tool_calls_started: usize,
    tool_calls_failed: usize,
    approvals_requested: usize,
    failures: &mut Vec<String>,
) {
    let expectations = &scenario.expectations;

    if let Some(expected_completed) = expectations.turn_completed {
        let actual_completed = turn_status == "completed";
        if actual_completed != expected_completed {
            failures.push(format!(
                "expected turn {} but it {}",
                if expected_completed {
                    "completed"
                } else {
                    "failed"
                },
                turn_status
            ));
        }
    }

    if let Some(expected) = &expectations.final_text_equals
        && final_text != *expected
    {
        failures.push(format!(
            "final answer mismatch:\nexpected: {expected:?}\nactual:   {final_text:?}"
        ));
    }

    for expected in &expectations.final_text_contains {
        if !final_text.contains(expected.as_str()) {
            failures.push(format!(
                "final answer does not contain {expected:?}: {final_text:?}"
            ));
        }
    }

    if let Some(expected) = &expectations.assistant_reasoning_equals
        && reasoning != *expected
    {
        failures.push(format!(
            "reasoning mismatch: expected {expected:?}, got {reasoning:?}"
        ));
    }

    for expected in &expectations.error_contains {
        let error = turn_error.unwrap_or_default();
        if !error.contains(expected.as_str()) {
            failures.push(format!(
                "turn error does not contain {expected:?}: {error:?}"
            ));
        }
    }

    if let Some(expected_sequence) = &expectations.tool_sequence
        && tool_sequence != *expected_sequence
    {
        failures.push(format!(
            "tool call sequence mismatch: expected {expected_sequence:?}, got {tool_sequence:?}"
        ));
    }

    for (actual, expected, label) in [
        (model_calls, expectations.model_calls, "model calls"),
        (
            tool_calls_started,
            expectations.tool_calls_started,
            "tool calls",
        ),
        (
            tool_calls_failed,
            expectations.tool_calls_failed,
            "failed tool calls",
        ),
    ] {
        if let Some(expected) = expected
            && actual != expected
        {
            failures.push(format!(
                "{label} mismatch: expected {expected}, got {actual}"
            ));
        }
    }

    if let Some(expected_roles) = &expectations.message_roles {
        let actual_roles: Vec<_> = messages.iter().map(|message| message.role).collect();
        if actual_roles != *expected_roles {
            failures.push(format!(
                "record message roles mismatch: expected {expected_roles:?}, got {actual_roles:?}"
            ));
        }
    }

    for assertion in &expectations.model_request_contains {
        let Some(request) = requests.get(assertion.model_call_index) else {
            failures.push(format!(
                "model request assertion out of range: call {} requested but only {} requests recorded",
                assertion.model_call_index,
                requests.len()
            ));
            continue;
        };
        if !request_contains(request, &assertion.contains) {
            failures.push(format!(
                "model request {} does not contain {:?}",
                assertion.model_call_index, assertion.contains
            ));
        }
    }

    for model_call_index in &expectations.model_requests_without_tools {
        let Some(request) = requests.get(*model_call_index) else {
            failures.push(format!(
                "model request assertion out of range: call {model_call_index} requested but only {} requests recorded",
                requests.len()
            ));
            continue;
        };
        if !request.tool_definitions.is_empty() {
            failures.push(format!(
                "model request {model_call_index} carries {} tool definitions, expected none",
                request.tool_definitions.len()
            ));
        }
    }

    if let Some(expected) = expectations.approvals_requested
        && approvals_requested != expected
    {
        failures.push(format!(
            "approval count mismatch: expected {expected}, got {approvals_requested}"
        ));
    }
}

fn request_contains(request: &RecordedModelRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|content| content.contains(needle))
            || message
                .reasoning_content
                .as_deref()
                .is_some_and(|content| content.contains(needle))
            || message.tool_calls.as_deref().is_some_and(|calls| {
                serde_json::to_string(calls).is_ok_and(|serialized| serialized.contains(needle))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Expectations, ModelScript, ModelStep, ScenarioTool, ToolBehavior};
    use agent_core::ToolExecutionMode;

    fn scenario() -> Scenario {
        Scenario::new(
            "runner_test",
            "scripted runner smoke test",
            "read the file and tell me",
        )
        .with_tool(ScenarioTool::new(
            "read_file",
            "read a file",
            ToolExecutionMode::Concurrent,
            ToolBehavior::ok("42"),
        ))
        .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
            crate::model::tool_call("call-1", "read_file", r#"{"path":"a.txt"}"#),
        ])]))
        .with_script(ModelScript::new(vec![
            ModelStep::text("the file says 42"),
            ModelStep::completed(),
        ]))
        .with_expectations(
            Expectations::completed()
                .contains("42")
                .tool_sequence(vec!["read_file"])
                .request_contains(1, "42"),
        )
        .with_budget(crate::scenario::Budget::new(2, 1, 2_000))
    }

    #[tokio::test]
    async fn smoke_scenario_passes_and_records_metrics() {
        let metrics = run_scenario(&scenario()).await;
        assert!(
            metrics.passed,
            "scenario should pass: {:?}",
            metrics.all_failures().collect::<Vec<_>>()
        );
        assert_eq!(metrics.turn_status, "completed");
        assert_eq!(metrics.model_calls, 2);
        assert_eq!(metrics.tool_calls_started, 1);
        assert_eq!(metrics.tool_calls_ok, 1);
        assert_eq!(metrics.final_text, "the file says 42");
        assert!(metrics.estimated_tokens > 0);
    }

    #[tokio::test]
    async fn budget_regression_is_reported() {
        let scenario = scenario().with_budget(crate::scenario::Budget::new(1, 0, 0));
        let metrics = run_scenario(&scenario).await;
        assert!(!metrics.passed);
        assert_eq!(metrics.budget_failures.len(), 3);
    }

    #[tokio::test]
    async fn builtin_suite_passes_without_baseline() {
        let report = run_suite(&crate::builtin_suite(), None).await;
        assert!(
            report.is_green(),
            "built-in suite regressed:\n{}",
            report
                .results
                .iter()
                .filter(|metrics| !metrics.passed)
                .map(|metrics| {
                    format!(
                        "  {}: {}",
                        metrics.scenario_id,
                        metrics
                            .all_failures()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
