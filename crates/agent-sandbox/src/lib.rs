use agent_protocol::{ApprovalRequest, PermissionMode, PermissionProfile, ShellPolicy};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PermissionEvaluatorError {
    #[error("failed to canonicalize workspace root {path}: {source}")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny(String),
    Prompt(ApprovalRequest),
}

#[derive(Debug, Clone)]
pub struct PermissionEvaluator {
    root: PathBuf,
    profile: PermissionProfile,
}

impl PermissionEvaluator {
    pub fn new(
        root: impl Into<PathBuf>,
        profile: PermissionProfile,
    ) -> Result<Self, PermissionEvaluatorError> {
        let root = root.into();
        let root = root
            .canonicalize()
            .map_err(|source| PermissionEvaluatorError::Root { path: root, source })?;

        Ok(Self { root, profile })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profile(&self) -> PermissionProfile {
        self.profile
    }

    pub fn allows_paths_outside_workspace(&self) -> bool {
        self.profile.mode == PermissionMode::DangerFullAccess
    }

    pub fn resolve_existing_path(&self, input: &str) -> Result<PathBuf, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("path must not be empty".to_string());
        }

        let candidate = Path::new(input);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let path = path
            .canonicalize()
            .map_err(|err| format!("failed to resolve path {input:?}: {err}"))?;

        if !self.allows_paths_outside_workspace() && !path.starts_with(&self.root) {
            return Err(format!("path {input:?} is outside the workspace root"));
        }

        Ok(path)
    }

    pub fn shell_command_decision(
        &self,
        tool_call_id: &str,
        command: &str,
        timeout_secs: u64,
    ) -> PermissionDecision {
        match self.profile.shell {
            ShellPolicy::Allow => PermissionDecision::Allow,
            ShellPolicy::Deny => PermissionDecision::Deny(format!(
                "shell commands are denied by the active {} permission profile",
                self.profile.mode.as_str()
            )),
            ShellPolicy::Prompt => PermissionDecision::Prompt(ApprovalRequest::shell_command(
                approval_id_for_tool_call(tool_call_id),
                command,
                self.root.clone(),
                timeout_secs,
                "shell command requires approval",
            )),
        }
    }

    pub fn display_path(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative.display().to_string()
        }
    }
}

pub fn approval_id_for_tool_call(tool_call_id: &str) -> String {
    format!("approval-{tool_call_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{ApprovalAction, PermissionMode};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-sandbox-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn read_only_and_workspace_write_prompt_for_shell() {
        for mode in [PermissionMode::ReadOnly, PermissionMode::WorkspaceWrite] {
            let root = unique_dir(mode.as_str());
            let evaluator =
                PermissionEvaluator::new(&root, PermissionProfile::for_mode(mode)).expect("eval");

            let decision = evaluator.shell_command_decision("call_1", "cargo test", 30);

            let PermissionDecision::Prompt(request) = decision else {
                panic!("expected prompt decision for {mode:?}");
            };
            assert_eq!(request.id, "approval-call_1");
            assert_eq!(
                request.action,
                ApprovalAction::ShellCommand {
                    command: "cargo test".to_string(),
                    cwd: root.canonicalize().expect("canonical root"),
                    timeout_secs: 30,
                }
            );
        }
    }

    #[test]
    fn danger_full_access_allows_shell() {
        let root = unique_dir("danger-shell");
        let evaluator = PermissionEvaluator::new(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        )
        .expect("eval");

        assert_eq!(
            evaluator.shell_command_decision("call_1", "cargo test", 30),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn shell_override_can_deny() {
        let root = unique_dir("shell-deny");
        let evaluator = PermissionEvaluator::new(
            &root,
            PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Deny,
            },
        )
        .expect("eval");

        assert!(matches!(
            evaluator.shell_command_decision("call_1", "cargo test", 30),
            PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn non_danger_profiles_reject_paths_outside_workspace() {
        let root = unique_dir("restricted-root");
        let outside = root
            .parent()
            .expect("parent")
            .join("outside-morrow-sandbox.txt");
        fs::write(&outside, "secret").expect("write outside");
        let evaluator = PermissionEvaluator::new(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        )
        .expect("eval");

        let err = evaluator
            .resolve_existing_path(&outside.display().to_string())
            .expect_err("outside path must fail");

        assert!(err.contains("outside the workspace root"));
    }

    #[test]
    fn danger_full_access_allows_paths_outside_workspace() {
        let root = unique_dir("danger-root");
        let outside = root
            .parent()
            .expect("parent")
            .join("outside-morrow-sandbox-danger.txt");
        fs::write(&outside, "secret").expect("write outside");
        let evaluator = PermissionEvaluator::new(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        )
        .expect("eval");

        let path = evaluator
            .resolve_existing_path(&outside.display().to_string())
            .expect("outside path");

        assert_eq!(path, outside.canonicalize().expect("canonical outside"));
    }
}
