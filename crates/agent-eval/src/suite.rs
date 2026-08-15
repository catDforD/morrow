//! Built-in deterministic suite.
//!
//! Every scenario here pins one invariant of the `agent-core` turn loop. The
//! scripts are authored by hand, so they measure the *harness* (does the loop
//! pass tool output to the next model call? does an error reach the model?
//! does the round limit hold? is an approval really required before a
//! side-effecting call?) — never the intelligence of a live model. Live-model
//! quality evals are intentionally a different mode.

use crate::model::tool_call;
use crate::scenario::{
    ApprovalPolicy, Budget, Expectations, ModelScript, ModelStep, Scenario, ScenarioTool,
    ToolBehavior, ToolResponse,
};
use agent_core::ToolExecutionMode;
use agent_protocol::Role;

/// The suite that CI runs. Order is stable so reports diff cleanly.
pub fn builtin_suite() -> Vec<Scenario> {
    vec![
        answer_without_tools(),
        reads_two_files_in_order(),
        recovers_after_tool_error(),
        mixed_serial_and_concurrent_tools(),
        unknown_tool_error_flows_to_model(),
        approval_denied_flows_to_model(),
        approval_granted_executes_tool(),
        runaway_tools_hit_round_limit(),
        model_error_fails_turn(),
        truncated_stream_fails_turn(),
        duplicate_tool_call_id_rejected(),
        empty_tool_call_list_rejected(),
        reasoning_content_preserved(),
    ]
}

fn read_tool(name: &str, content: &str) -> ScenarioTool {
    ScenarioTool::new(
        name,
        format!("Reads {name}"),
        ToolExecutionMode::Concurrent,
        ToolBehavior::ok(content),
    )
}

fn answer_without_tools() -> Scenario {
    Scenario::new(
        "answer_without_tools",
        "A single model answer with no tools completes the turn.",
        "what is 6 * 7?",
    )
    .with_script(ModelScript::new(vec![
        ModelStep::text("Answer: 42"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("Answer: 42")
            .contains("42")
            .model_calls(1)
            .tool_calls_started(0)
            .message_roles(vec![Role::User, Role::Assistant]),
    )
    .with_budget(Budget::new(1, 0, 300))
}

fn reads_two_files_in_order() -> Scenario {
    Scenario::new(
        "reads_two_files_in_order",
        "Two concurrent tool results are fed back to the next model call in request order.",
        "read both files and combine them",
    )
    .with_tool(read_tool("read_a", "alpha"))
    .with_tool(read_tool("read_b", "beta"))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("read-a", "read_a", r#"{"path":"a.txt"}"#),
        tool_call("read-b", "read_b", r#"{"path":"b.txt"}"#),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("alpha|beta"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("alpha|beta")
            .tool_sequence(vec!["read_a", "read_b"])
            .model_calls(2)
            .tool_calls_started(2)
            .tool_calls_failed(0)
            .message_roles(vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Tool,
                Role::Assistant,
            ])
            .request_contains(1, "alpha")
            .request_contains(1, "beta"),
    )
    .with_budget(Budget::new(2, 2, 1_500))
}

fn recovers_after_tool_error() -> Scenario {
    Scenario::new(
        "recovers_after_tool_error",
        "A failed tool call is reported as a tool result and the loop keeps going.",
        "read the flaky file",
    )
    .with_tool(ScenarioTool::new(
        "flaky_read",
        "Reads a file that sometimes fails",
        ToolExecutionMode::Concurrent,
        ToolBehavior::sequence(vec![
            ToolResponse::Fail("connection reset".to_string()),
            ToolResponse::Ok("recovered content".to_string()),
        ]),
    ))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("flaky-1", "flaky_read", r#"{"path":"flaky.txt"}"#),
    ])]))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("flaky-2", "flaky_read", r#"{"path":"flaky.txt"}"#),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("recovered content"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("recovered content")
            .tool_sequence(vec!["flaky_read", "flaky_read"])
            .model_calls(3)
            .tool_calls_started(2)
            .tool_calls_failed(1)
            .message_roles(vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
            ])
            .request_contains(2, "recovered content"),
    )
    .with_budget(Budget::new(3, 2, 1_800))
}

fn mixed_serial_and_concurrent_tools() -> Scenario {
    Scenario::new(
        "mixed_serial_and_concurrent_tools",
        "Concurrent and serial tools in one batch both execute and their results round-trip.",
        "read the note and update it",
    )
    .with_tool(read_tool("read_note", "before"))
    .with_tool(ScenarioTool::new(
        "write_note",
        "Writes the note",
        ToolExecutionMode::Serial,
        ToolBehavior::ok("written"),
    ))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("read-1", "read_note", r#"{"path":"note.txt"}"#),
        tool_call(
            "write-1",
            "write_note",
            r#"{"path":"note.txt","content":"after"}"#,
        ),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("before,written"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("before,written")
            .tool_sequence(vec!["read_note", "write_note"])
            .tool_calls_started(2)
            .tool_calls_failed(0)
            .request_contains(1, "before")
            .request_contains(1, "written"),
    )
    .with_budget(Budget::new(2, 2, 1_800))
}

fn unknown_tool_error_flows_to_model() -> Scenario {
    Scenario::new(
        "unknown_tool_error_flows_to_model",
        "Calling an unknown tool produces a tool error the model can respond to.",
        "do the unknown thing",
    )
    .with_tool(read_tool("known_tool", "known"))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("unknown-1", "does_not_exist", "{}"),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("unknown tool is unavailable"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .contains("unavailable")
            .tool_sequence(vec!["does_not_exist"])
            .tool_calls_started(1)
            .tool_calls_failed(1)
            .request_contains(1, "unknown tool"),
    )
    .with_budget(Budget::new(2, 1, 1_200))
}

fn approval_denied_flows_to_model() -> Scenario {
    Scenario::new(
        "approval_denied_flows_to_model",
        "Denying an approval turns the tool call into a tool error in the next model request.",
        "write the file",
    )
    .with_tool(ScenarioTool::new(
        "write_file",
        "Writes a file; requires approval",
        ToolExecutionMode::Serial,
        ToolBehavior::Always(ToolResponse::Approval {
            approved: "written".to_string(),
            denied: "denied by user".to_string(),
        }),
    ))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("write-1", "write_file", r#"{"path":"f.txt"}"#),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("write blocked"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("write blocked")
            .tool_sequence(vec!["write_file"])
            .tool_calls_started(1)
            .tool_calls_failed(1)
            .approvals_requested(1)
            .request_contains(1, "approval denied"),
    )
    .with_approval_policy(ApprovalPolicy::Deny)
    .with_budget(Budget::new(2, 1, 1_400))
}

fn approval_granted_executes_tool() -> Scenario {
    Scenario::new(
        "approval_granted_executes_tool",
        "Approving an approval request executes the tool and its result reaches the model.",
        "write the file and confirm",
    )
    .with_tool(ScenarioTool::new(
        "write_file",
        "Writes a file; requires approval",
        ToolExecutionMode::Serial,
        ToolBehavior::Always(ToolResponse::Approval {
            approved: "written ok".to_string(),
            denied: "denied by user".to_string(),
        }),
    ))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("write-1", "write_file", r#"{"path":"f.txt"}"#),
    ])]))
    .with_script(ModelScript::new(vec![
        ModelStep::text("written ok"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("written ok")
            .tool_sequence(vec!["write_file"])
            .tool_calls_started(1)
            .tool_calls_failed(0)
            .approvals_requested(1)
            .request_contains(1, "written ok"),
    )
    .with_approval_policy(ApprovalPolicy::Approve)
    .with_budget(Budget::new(2, 1, 1_400))
}

fn runaway_tools_hit_round_limit() -> Scenario {
    Scenario::new(
        "runaway_tools_hit_round_limit",
        "An agent stuck in a tool loop is cut off by the configured round limit.",
        "keep pinging until I say stop",
    )
    .with_tool(ScenarioTool::new(
        "ping",
        "Pings back",
        ToolExecutionMode::Concurrent,
        ToolBehavior::ok("pong"),
    ))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("ping-1", "ping", "{}"),
    ])]))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("ping-2", "ping", "{}"),
    ])]))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("ping-3", "ping", "{}"),
    ])]))
    .with_max_tool_rounds(2)
    .with_expectations(
        Expectations::failed()
            .error_contains("round limit")
            .tool_sequence(vec!["ping", "ping"])
            .model_calls(3)
            .tool_calls_started(2)
            .tool_calls_failed(0),
    )
    .with_budget(Budget::new(3, 2, 1_500))
}

fn model_error_fails_turn() -> Scenario {
    Scenario::new(
        "model_error_fails_turn",
        "A provider error surfaces as a failed turn with the original message.",
        "answer something",
    )
    .with_script(ModelScript::new(vec![ModelStep::error(
        "upstream provider returned 500",
    )]))
    .with_expectations(
        Expectations::failed()
            .error_contains("upstream provider returned 500")
            .model_calls(1)
            .tool_calls_started(0),
    )
    .with_budget(Budget::new(1, 0, 300))
}

fn truncated_stream_fails_turn() -> Scenario {
    Scenario::new(
        "truncated_stream_fails_turn",
        "A model stream that ends without a completion marker fails the turn explicitly.",
        "answer something",
    )
    .with_script(ModelScript::new(vec![
        ModelStep::text("partial answer"),
        ModelStep::truncate(),
    ]))
    .with_expectations(
        Expectations::failed()
            .error_contains("stream ended before completion")
            .model_calls(1)
            .tool_calls_started(0),
    )
    .with_budget(Budget::new(1, 0, 300))
}

fn duplicate_tool_call_id_rejected() -> Scenario {
    Scenario::new(
        "duplicate_tool_call_id_rejected",
        "Two tool calls with the same id fail the turn instead of corrupting history.",
        "call the tool twice",
    )
    .with_tool(read_tool("read_file", "content"))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(vec![
        tool_call("dup-1", "read_file", "{}"),
        tool_call("dup-1", "read_file", "{}"),
    ])]))
    .with_expectations(
        Expectations::failed()
            .error_contains("duplicate tool call id")
            .model_calls(1)
            .tool_calls_started(0),
    )
    .with_budget(Budget::new(1, 0, 800))
}

fn empty_tool_call_list_rejected() -> Scenario {
    Scenario::new(
        "empty_tool_call_list_rejected",
        "A model response that requests tools but names none fails the turn explicitly.",
        "do something ambiguous",
    )
    .with_tool(read_tool("read_file", "content"))
    .with_script(ModelScript::new(vec![ModelStep::tool_calls(Vec::new())]))
    .with_expectations(
        Expectations::failed()
            .error_contains("did not provide any tool call")
            .model_calls(1)
            .tool_calls_started(0),
    )
    .with_budget(Budget::new(1, 0, 800))
}

fn reasoning_content_preserved() -> Scenario {
    Scenario::new(
        "reasoning_content_preserved",
        "Reasoning deltas are committed to the final assistant message.",
        "think out loud",
    )
    .with_script(ModelScript::new(vec![
        ModelStep::reasoning("step-by-step thinking"),
        ModelStep::text("done"),
        ModelStep::completed(),
    ]))
    .with_expectations(
        Expectations::completed()
            .equals("done")
            .reasoning_equals("step-by-step thinking")
            .model_calls(1),
    )
    .with_budget(Budget::new(1, 0, 400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_suite_scenarios_are_structurally_valid() {
        for scenario in builtin_suite() {
            if let Err(errors) = scenario.validate() {
                panic!("invalid scenario {}: {errors:?}", scenario.id);
            }
        }
    }

    #[test]
    fn builtin_suite_ids_are_unique() {
        let suite = builtin_suite();
        let mut seen = std::collections::HashSet::new();
        for scenario in &suite {
            assert!(
                seen.insert(scenario.id.as_str()),
                "duplicate scenario id {}",
                scenario.id
            );
        }
    }
}
