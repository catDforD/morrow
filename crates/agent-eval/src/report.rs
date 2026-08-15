use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const EVAL_REPORT_SCHEMA_VERSION: u32 = 1;
pub const EVAL_BASELINE_SCHEMA_VERSION: u32 = 1;

/// Measured, deterministic metrics for one scenario run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMetrics {
    pub scenario_id: String,
    pub passed: bool,
    /// `completed` or `failed`, taken from the persisted turn record.
    pub turn_status: String,
    pub final_text: String,
    pub turn_error: Option<String>,
    pub model_calls: usize,
    pub tool_calls_started: usize,
    pub tool_calls_ok: usize,
    pub tool_calls_failed: usize,
    pub approvals_requested: usize,
    pub approvals_resolved: usize,
    /// Deterministic token proxy. This is a stable heuristic (roughly
    /// `chars / 4` plus per-message and per-tool overhead), not provider
    /// billing — its job is regression detection, not invoicing.
    pub estimated_tokens: usize,
    pub duration_ms: u64,
    pub assertion_failures: Vec<String>,
    pub budget_failures: Vec<String>,
    pub baseline_failures: Vec<String>,
}

impl ScenarioMetrics {
    pub fn all_failures(&self) -> impl Iterator<Item = &String> {
        self.assertion_failures
            .iter()
            .chain(&self.budget_failures)
            .chain(&self.baseline_failures)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteReport {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub scenario_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub results: Vec<ScenarioMetrics>,
}

impl SuiteReport {
    pub fn is_green(&self) -> bool {
        self.failed == 0
    }
}

/// Stored budget ceilings. A code change may only *consume less* than the
/// baseline without touching it; raising a ceiling is an explicit
/// `--update-baseline` decision made after the scenario still passes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    pub updated_at_ms: u64,
    pub budgets: BTreeMap<String, BaselineBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineBudget {
    pub max_model_calls: usize,
    pub max_tool_calls: usize,
    pub max_estimated_tokens: usize,
}

impl Baseline {
    pub fn load(path: &Path) -> Result<Self, BaselineError> {
        let bytes = std::fs::read(path).map_err(|source| BaselineError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let baseline: Baseline =
            serde_json::from_slice(&bytes).map_err(|source| BaselineError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if baseline.schema_version != EVAL_BASELINE_SCHEMA_VERSION {
            return Err(BaselineError::UnsupportedSchema {
                path: path.to_path_buf(),
                version: baseline.schema_version,
            });
        }
        Ok(baseline)
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        let parent = path.parent().ok_or_else(|| BaselineError::NoParent {
            path: path.to_path_buf(),
        })?;
        std::fs::create_dir_all(parent).map_err(|source| BaselineError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self).map_err(BaselineError::Serialize)?;
        std::fs::write(&temporary, bytes).map_err(|source| BaselineError::Write {
            path: temporary.clone(),
            source,
        })?;
        // `rename` does not replace an existing target on Windows.
        if path.exists() {
            std::fs::remove_file(path).map_err(|source| BaselineError::Remove {
                path: path.to_path_buf(),
                source,
            })?;
        }
        std::fs::rename(&temporary, path).map_err(|source| BaselineError::Replace {
            source: temporary,
            target: path.to_path_buf(),
            source_error: source,
        })?;
        Ok(())
    }

    /// Compare measured metrics against the stored ceilings. Returns one
    /// failure string per exceeded budget.
    pub fn check(&self, scenario_id: &str, metrics: &ScenarioMetrics) -> Vec<String> {
        let Some(budget) = self.budgets.get(scenario_id) else {
            return Vec::new();
        };
        let mut failures = Vec::new();
        if metrics.model_calls > budget.max_model_calls {
            failures.push(format!(
                "baseline regression: model_calls {} exceeds baseline {}",
                metrics.model_calls, budget.max_model_calls
            ));
        }
        if metrics.tool_calls_started > budget.max_tool_calls {
            failures.push(format!(
                "baseline regression: tool_calls {} exceeds baseline {}",
                metrics.tool_calls_started, budget.max_tool_calls
            ));
        }
        if metrics.estimated_tokens > budget.max_estimated_tokens {
            failures.push(format!(
                "baseline regression: estimated_tokens {} exceeds baseline {}",
                metrics.estimated_tokens, budget.max_estimated_tokens
            ));
        }
        failures
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error("failed to read baseline {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse baseline {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported baseline schema version {version} in {path}")]
    UnsupportedSchema {
        path: std::path::PathBuf,
        version: u32,
    },
    #[error("baseline path {path} has no parent directory")]
    NoParent { path: std::path::PathBuf },
    #[error("failed to create baseline directory {path}: {source}")]
    CreateDir {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize baseline: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to write baseline {path}: {source}")]
    Write {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove existing baseline {path}: {source}")]
    Remove {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace baseline {source} with {target}: {source_error}")]
    Replace {
        source: std::path::PathBuf,
        target: std::path::PathBuf,
        #[source]
        source_error: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(
        scenario_id: &str,
        model_calls: usize,
        tool_calls: usize,
        tokens: usize,
    ) -> ScenarioMetrics {
        ScenarioMetrics {
            scenario_id: scenario_id.to_string(),
            passed: true,
            turn_status: "completed".to_string(),
            final_text: "done".to_string(),
            turn_error: None,
            model_calls,
            tool_calls_started: tool_calls,
            tool_calls_ok: tool_calls,
            tool_calls_failed: 0,
            approvals_requested: 0,
            approvals_resolved: 0,
            estimated_tokens: tokens,
            duration_ms: 1,
            assertion_failures: Vec::new(),
            budget_failures: Vec::new(),
            baseline_failures: Vec::new(),
        }
    }

    #[test]
    fn baseline_check_flags_each_regression_dimension() {
        let baseline = Baseline {
            schema_version: EVAL_BASELINE_SCHEMA_VERSION,
            updated_at_ms: 1,
            budgets: BTreeMap::from([(
                "s".to_string(),
                BaselineBudget {
                    max_model_calls: 2,
                    max_tool_calls: 2,
                    max_estimated_tokens: 100,
                },
            )]),
        };

        let failures = baseline.check("s", &metrics("s", 3, 3, 101));
        assert_eq!(failures.len(), 3);
        assert!(failures[0].contains("model_calls"));
        assert!(failures[1].contains("tool_calls"));
        assert!(failures[2].contains("estimated_tokens"));

        assert!(baseline.check("s", &metrics("s", 2, 2, 100)).is_empty());
        assert!(baseline.check("s", &metrics("s", 1, 1, 10)).is_empty());
        assert!(
            baseline
                .check("unknown", &metrics("unknown", 99, 99, 99_999))
                .is_empty()
        );
    }

    #[test]
    fn baseline_round_trips_through_json() {
        let baseline = Baseline {
            schema_version: EVAL_BASELINE_SCHEMA_VERSION,
            updated_at_ms: 42,
            budgets: BTreeMap::from([(
                "s".to_string(),
                BaselineBudget {
                    max_model_calls: 2,
                    max_tool_calls: 3,
                    max_estimated_tokens: 400,
                },
            )]),
        };
        let encoded = serde_json::to_vec(&baseline).expect("encode");
        let decoded: Baseline = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded.schema_version, EVAL_BASELINE_SCHEMA_VERSION);
        assert_eq!(decoded.updated_at_ms, 42);
        assert_eq!(
            decoded.budgets["s"].max_estimated_tokens,
            baseline.budgets["s"].max_estimated_tokens
        );
    }
}
