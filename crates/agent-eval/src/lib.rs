//! Deterministic, model-independent evaluation harness for the Morrow agent
//! loop.
//!
//! `agent-eval` runs `agent-core` end to end against a scripted model and
//! scripted tools, then asserts on observable facts: turn status, final
//! answer, tool call sequence, message chain, approvals, model requests and
//! efficiency budgets. No live model and no network are involved, so the same
//! scenario always produces the same metrics.
//!
//! ```bash
//! cargo run -p agent-eval -- run                 # CI gate
//! cargo run -p agent-eval -- run --update-baseline
//! cargo run -p agent-eval -- list
//! ```

pub mod model;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod suite;
pub mod tools;

pub use report::{Baseline, BaselineBudget, BaselineError, ScenarioMetrics, SuiteReport};
pub use runner::{now_ms, run_scenario, run_suite};
pub use scenario::{
    ApprovalPolicy, Budget, Expectations, ModelScript, ModelStep, RequestAssertion, Scenario,
    ScenarioTool, ToolBehavior, ToolResponse,
};
pub use suite::builtin_suite;

/// Run the built-in suite, optionally comparing against stored budget
/// ceilings.
pub async fn run_builtin_suite(baseline: Option<&report::Baseline>) -> SuiteReport {
    run_suite(&builtin_suite(), baseline).await
}
