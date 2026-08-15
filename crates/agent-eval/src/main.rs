use agent_eval::report::{Baseline, BaselineBudget, SuiteReport};
use agent_eval::{builtin_suite, now_ms, run_suite};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/baselines/v1.json");

#[derive(Debug, Parser)]
#[command(
    name = "agent-eval",
    version,
    about = "Deterministic, model-independent regression suite for the Morrow agent loop"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the built-in scenario suite.
    Run(RunArgs),
    /// List built-in scenarios.
    List,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Baseline budget file used as the regression ceiling.
    #[arg(long, default_value = DEFAULT_BASELINE)]
    baseline: PathBuf,
    /// Re-record the baseline from this run. Refused when any scenario fails
    /// its own assertions or budget, so a broken agent can never ratify itself.
    #[arg(long)]
    update_baseline: bool,
    /// Write the JSON report to this path.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Run only scenarios whose id contains this string (repeatable).
    #[arg(long)]
    scenario: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::List => list(),
        Command::Run(args) => run(args).await,
    }
}

fn list() -> ExitCode {
    let suite = builtin_suite();
    println!("built-in scenarios ({}):", suite.len());
    for scenario in &suite {
        println!("  {:<40} {}", scenario.id, scenario.description);
    }
    ExitCode::SUCCESS
}

async fn run(args: RunArgs) -> ExitCode {
    let mut scenarios = builtin_suite();
    if !args.scenario.is_empty() {
        scenarios.retain(|scenario| {
            args.scenario
                .iter()
                .any(|filter| scenario.id.contains(filter.as_str()))
        });
        if scenarios.is_empty() {
            eprintln!("no scenarios match filter(s): {}", args.scenario.join(", "));
            return ExitCode::from(2);
        }
    }

    // Updates are judged against the scenario's own budgets only: the old
    // baseline must not veto the very edit that raises a ceiling. But the old
    // budget entries for *unselected* scenarios are kept, so a filtered
    // `--update-baseline` never silently drops their ceilings.
    let (comparison_baseline, existing_baseline) = if args.update_baseline {
        let existing = Baseline::load(&args.baseline).ok();
        (None, existing)
    } else {
        match Baseline::load(&args.baseline) {
            Ok(baseline) => (Some(baseline), None),
            Err(error) => {
                eprintln!(
                    "failed to load baseline {}: {error}",
                    args.baseline.display()
                );
                eprintln!(
                    "hint: run `cargo run -p agent-eval -- run --update-baseline` once after authoring scenarios"
                );
                return ExitCode::from(2);
            }
        }
    };

    let report = run_suite(&scenarios, comparison_baseline.as_ref()).await;
    print_report(&report);

    if args.update_baseline {
        return update_baseline(args, report, existing_baseline);
    }

    if let Some(path) = args.report
        && let Err(error) = write_report(&report, &path)
    {
        eprintln!("failed to write report {}: {error}", path.display());
        return ExitCode::from(2);
    }

    if report.is_green() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn update_baseline(args: RunArgs, report: SuiteReport, existing: Option<Baseline>) -> ExitCode {
    let broken: Vec<_> = report
        .results
        .iter()
        .filter(|metrics| {
            !metrics.assertion_failures.is_empty() || !metrics.budget_failures.is_empty()
        })
        .collect();
    if !broken.is_empty() {
        eprintln!("refusing to update baseline: scenarios failed their own criteria:");
        for metrics in &broken {
            eprintln!("  {}", metrics.scenario_id);
            for failure in metrics
                .assertion_failures
                .iter()
                .chain(&metrics.budget_failures)
            {
                eprintln!("    - {failure}");
            }
        }
        return ExitCode::FAILURE;
    }

    let mut budgets = existing
        .map(|baseline| baseline.budgets)
        .unwrap_or_default();
    for metrics in &report.results {
        budgets.insert(
            metrics.scenario_id.clone(),
            BaselineBudget {
                max_model_calls: metrics.model_calls,
                max_tool_calls: metrics.tool_calls_started,
                max_estimated_tokens: metrics.estimated_tokens,
            },
        );
    }
    let baseline = Baseline {
        schema_version: agent_eval::report::EVAL_BASELINE_SCHEMA_VERSION,
        updated_at_ms: now_ms(),
        budgets,
    };

    match baseline.save(&args.baseline) {
        Ok(()) => {
            println!(
                "baseline updated: {} scenario budget ceilings written to {}",
                baseline.budgets.len(),
                args.baseline.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("failed to save baseline: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_report(report: &SuiteReport) {
    for metrics in &report.results {
        println!(
            "{:<6} {:<40} {:<10} model={:<3} tools={:<3} tokens={}",
            if metrics.passed { "PASS" } else { "FAIL" },
            metrics.scenario_id,
            metrics.turn_status,
            metrics.model_calls,
            metrics.tool_calls_started,
            metrics.estimated_tokens,
        );
        for failure in metrics.all_failures() {
            println!("         {failure}");
        }
    }
    println!(
        "summary: {} passed, {} failed ({} scenarios)",
        report.passed, report.failed, report.scenario_count
    );
}

fn write_report(report: &SuiteReport, path: &PathBuf) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}
