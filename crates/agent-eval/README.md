# agent-eval

Deterministic, model-independent regression suite for the Morrow agent loop.

## Why this crate exists

`cargo test` proves components don't crash. It does **not** prove the agent
still works after we change the turn loop, the tool pipeline, approvals or
context handling. `agent-eval` closes that gap by running the real
`agent-core` turn loop end to end against:

- a **scripted model** (`ScriptedModel`) that replays a fixed response script
  and records every request it receives, and
- a **scripted tool runtime** (`ScenarioToolRuntime`) whose behavior is
  authored per scenario and whose calls are all recorded.

No live model, no network, no sampling. The same scenario always produces the
same events and metrics, so CI failures are reproducible by definition.

## What is measured

Each scenario asserts on observable facts:

| Signal | Example |
| --- | --- |
| Turn outcome | must complete / must fail |
| Final answer | exact text or required substrings |
| Tool call sequence | `["read_a", "read_b"]` in that order |
| Message chain roles | `user -> assistant(tool_calls) -> tool -> assistant(final)` |
| Model requests | tool result `"alpha"` actually appears in model call 1 |
| Approvals | exactly one approval surfaced, then granted/denied |
| Error propagation | provider 500, stream truncation, duplicate ids |
| Efficiency budgets | max model calls, max tool calls, estimated tokens |

The efficiency budgets are the ratchet that prevents "the agent still works,
but now takes 3 extra model rounds to do it" from slipping into main.

## How the ratchet works

1. Every scenario declares a static `Budget` (hard upper bound) and must pass
   its behavioral assertions.
2. `baselines/v1.json` stores measured ceilings. On every `run`, measured
   model calls / tool calls / estimated tokens must be **at or below** the
   baseline.
3. `--update-baseline` re-records ceilings **only if every scenario passes its
   own assertions and budget**. A broken agent can never ratify itself.

The result: code may consume less (fine) or the same, but consuming more
requires a deliberate, reviewed baseline bump — and even then the static
budget and behavioral assertions still apply.

## Usage

```bash
# CI gate (from the repository root)
cargo run -p agent-eval -- run

# Re-record efficiency ceilings after an intentional, verified change
cargo run -p agent-eval -- run --update-baseline

# One scenario, JSON report on the side
cargo run -p agent-eval -- run --scenario recovers_after_tool_error --report /tmp/report.json

# Inspect the suite
cargo run -p agent-eval -- list
```

Exit code is `0` only when every selected scenario passes; CI fails on any
behavioral or budget regression.

## Adding a scenario

Add one function in `crates/agent-eval/src/suite.rs` and append it to
`builtin_suite()`. Example:

```rust
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
            .request_contains(2, "recovered content"),
    )
    .with_budget(Budget::new(3, 2, 1_800))
}
```

Rules for authoring:

- Script exactly what the loop should do; never script what a "smart model"
  would do. The model's IQ is deliberately not under test here.
- Make every assertion observable: turn record, events, recorded requests or
  recorded tool calls.
- Keep budgets tight but not hair-trigger; measured values are printed on
  every run, so start slightly generous and tighten with `--update-baseline`.
- A scenario with `ModelStep::ToolCalls` must be the last step of its model
  response (the loop stops reading that stream). Use `ModelStep::Truncate` to
  explicitly script a stream that ends without a completion marker.
- Run `cargo run -p agent-eval -- run --update-baseline` once after adding
  scenarios and commit the baseline diff.

## Non-goals (for now)

Live-model quality evaluation (task success rate with a real LLM), latency
benchmarks, and provider-compatibility runs are deliberate non-goals for this
crate. They belong in a separate opt-in harness because they are slow, costly,
and non-deterministic. `agent-eval` is the CI-safe inner loop; a live-model
harness can later be the outer loop.
