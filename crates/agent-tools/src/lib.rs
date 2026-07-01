use agent_protocol::{
    ApprovalDecision, ApprovalRequest, PermissionProfile, ToolCall, ToolDefinition,
};
use agent_sandbox::{PermissionDecision, PermissionEvaluator, PermissionEvaluatorError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 1000;
const DEFAULT_LIST_ENTRIES: usize = 100;
const MAX_LIST_ENTRIES: usize = 500;
const DEFAULT_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_RESULTS: usize = 200;
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 30;
const MAX_SHELL_TIMEOUT_SECS: u64 = 120;
const MAX_SHELL_OUTPUT_BYTES: usize = 20_000;

#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error(transparent)]
    PermissionEvaluator(#[from] PermissionEvaluatorError),
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    evaluator: Option<PermissionEvaluator>,
    definitions: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecution {
    Completed(ToolResult),
    ApprovalRequired(ApprovalRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub ok: bool,
    pub content: String,
    pub error: Option<String>,
}

impl ToolExecution {
    pub fn error(error: impl Into<String>) -> Self {
        Self::Completed(tool_error(error.into()))
    }
}

impl ToolResult {
    pub fn error(error: impl Into<String>) -> Self {
        tool_error(error.into())
    }
}

impl ToolRegistry {
    pub fn empty() -> Self {
        Self {
            evaluator: None,
            definitions: Vec::new(),
        }
    }

    pub fn built_in(
        root: impl Into<PathBuf>,
        permissions: PermissionProfile,
    ) -> Result<Self, ToolRegistryError> {
        let evaluator = PermissionEvaluator::new(root, permissions)?;

        Ok(Self {
            evaluator: Some(evaluator),
            definitions: built_in_definitions(),
        })
    }

    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub fn execute(&self, call: &ToolCall) -> ToolExecution {
        self.execute_inner(call, None)
    }

    pub fn execute_approved(&self, call: &ToolCall, decision: &ApprovalDecision) -> ToolExecution {
        self.execute_inner(call, Some(decision))
    }

    fn execute_inner(&self, call: &ToolCall, approval: Option<&ApprovalDecision>) -> ToolExecution {
        let result = match call.function.name.as_str() {
            "read_file" => self.read_file(call).map(tool_ok),
            "list_files" => self.list_files(call).map(tool_ok),
            "search_text" => self.search_text(call).map(tool_ok),
            "shell_command" => return self.shell_command(call, approval),
            name => Err(format!("unknown tool {name:?}")),
        };

        match result {
            Ok(result) => ToolExecution::Completed(result),
            Err(error) => ToolExecution::Completed(tool_error(error)),
        }
    }

    fn read_file(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<ReadFileArgs>(call)?;
        let start_line = args.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err("start_line must be at least 1".to_string());
        }
        let max_lines = clamp_limit(args.max_lines, DEFAULT_READ_LINES, MAX_READ_LINES)?;
        let path = self.resolve_existing_path(&args.path)?;
        if !path.is_file() {
            return Err(format!("{} is not a file", self.display_path(&path)));
        }

        let content = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", self.display_path(&path)))?;
        let lines = content.lines().collect::<Vec<_>>();
        let selected = lines
            .iter()
            .skip(start_line.saturating_sub(1))
            .take(max_lines)
            .copied()
            .collect::<Vec<_>>();
        let end_line = (!selected.is_empty()).then_some(start_line + selected.len() - 1);
        let truncated = start_line.saturating_sub(1) + selected.len() < lines.len();

        Ok(json!({
            "path": self.display_path(&path),
            "start_line": start_line,
            "end_line": end_line,
            "total_lines": lines.len(),
            "truncated": truncated,
            "content": selected.join("\n"),
        }))
    }

    fn list_files(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<ListFilesArgs>(call)?;
        let path = args.path.unwrap_or_else(|| ".".to_string());
        let recursive = args.recursive.unwrap_or(false);
        let max_entries = clamp_limit(args.max_entries, DEFAULT_LIST_ENTRIES, MAX_LIST_ENTRIES)?;
        let path = self.resolve_existing_path(&path)?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory", self.display_path(&path)));
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        self.collect_entries(&path, recursive, max_entries, &mut entries, &mut truncated)?;

        Ok(json!({
            "path": self.display_path(&path),
            "recursive": recursive,
            "truncated": truncated,
            "entries": entries,
        }))
    }

    fn search_text(&self, call: &ToolCall) -> Result<Value, String> {
        let args = parse_args::<SearchTextArgs>(call)?;
        if args.query.is_empty() {
            return Err("query must not be empty".to_string());
        }
        let path = args.path.unwrap_or_else(|| ".".to_string());
        let path = self.resolve_existing_path(&path)?;
        let max_results =
            clamp_limit(args.max_results, DEFAULT_SEARCH_RESULTS, MAX_SEARCH_RESULTS)?;
        let case_sensitive = args.case_sensitive.unwrap_or(false);
        let mut results = Vec::new();
        let mut truncated = false;

        if path.is_file() {
            let options = SearchOptions {
                query: &args.query,
                case_sensitive,
                max_results,
                fail_on_read_error: true,
            };
            self.search_file(&path, &options, &mut results, &mut truncated)?;
        } else if path.is_dir() {
            let mut files = Vec::new();
            self.collect_search_files(&path, &mut files)?;
            let options = SearchOptions {
                query: &args.query,
                case_sensitive,
                max_results,
                fail_on_read_error: false,
            };
            for file in files {
                self.search_file(&file, &options, &mut results, &mut truncated)?;
                if truncated {
                    break;
                }
            }
        } else {
            return Err(format!("{} is not searchable", self.display_path(&path)));
        }

        Ok(json!({
            "query": args.query,
            "path": self.display_path(&path),
            "case_sensitive": case_sensitive,
            "truncated": truncated,
            "results": results,
        }))
    }

    fn shell_command(&self, call: &ToolCall, approval: Option<&ApprovalDecision>) -> ToolExecution {
        let args = match parse_args::<ShellCommandArgs>(call) {
            Ok(args) => args,
            Err(error) => return ToolExecution::error(error),
        };
        if args.command.trim().is_empty() {
            return ToolExecution::error("command must not be empty");
        }
        let timeout_secs = args
            .timeout_secs
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS)
            .min(MAX_SHELL_TIMEOUT_SECS);
        if timeout_secs == 0 {
            return ToolExecution::error("timeout_secs must be at least 1");
        }

        let evaluator = match self.evaluator() {
            Ok(evaluator) => evaluator,
            Err(error) => return ToolExecution::error(error),
        };

        match evaluator.shell_command_decision(&call.id, &args.command, timeout_secs) {
            PermissionDecision::Allow => complete_tool_result(run_shell_command(
                evaluator.root(),
                &args.command,
                Duration::from_secs(timeout_secs),
            )),
            PermissionDecision::Deny(error) => ToolExecution::error(error),
            PermissionDecision::Prompt(request) => match approval {
                None => ToolExecution::ApprovalRequired(request),
                Some(decision) if decision.request_id != request.id => {
                    ToolExecution::error(format!(
                        "approval decision {} does not match required approval {}",
                        decision.request_id, request.id
                    ))
                }
                Some(decision) if !decision.approved => {
                    ToolExecution::error("shell command approval denied")
                }
                Some(_) => complete_tool_result(run_shell_command(
                    evaluator.root(),
                    &args.command,
                    Duration::from_secs(timeout_secs),
                )),
            },
        }
    }

    fn resolve_existing_path(&self, input: &str) -> Result<PathBuf, String> {
        self.evaluator()?.resolve_existing_path(input)
    }

    fn display_path(&self, path: &Path) -> String {
        self.evaluator()
            .map(|evaluator| evaluator.display_path(path))
            .unwrap_or_else(|_| path.display().to_string())
    }

    fn evaluator(&self) -> Result<&PermissionEvaluator, String> {
        self.evaluator
            .as_ref()
            .ok_or_else(|| "built-in tools are not available".to_string())
    }

    fn path_allowed(&self, path: &Path) -> Result<bool, String> {
        let evaluator = self.evaluator()?;
        Ok(evaluator.allows_paths_outside_workspace() || path.starts_with(evaluator.root()))
    }

    fn collect_entries(
        &self,
        dir: &Path,
        recursive: bool,
        max_entries: usize,
        entries: &mut Vec<Value>,
        truncated: &mut bool,
    ) -> Result<(), String> {
        let mut dir_entries = fs::read_dir(dir)
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?;
        dir_entries.sort_by_key(|entry| entry.file_name());

        for entry in dir_entries {
            if should_skip_entry(&entry.path()) {
                continue;
            }
            if entries.len() >= max_entries {
                *truncated = true;
                return Ok(());
            }

            let path = entry
                .path()
                .canonicalize()
                .map_err(|err| format!("failed to resolve listed path: {err}"))?;
            if !self.path_allowed(&path)? {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
            let kind = if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(json!({
                "path": self.display_path(&path),
                "kind": kind,
            }));

            if recursive && file_type.is_dir() {
                self.collect_entries(&path, recursive, max_entries, entries, truncated)?;
                if *truncated {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn collect_search_files(&self, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut dir_entries = fs::read_dir(dir)
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to list {}: {err}", self.display_path(dir)))?;
        dir_entries.sort_by_key(|entry| entry.file_name());

        for entry in dir_entries {
            if should_skip_entry(&entry.path()) {
                continue;
            }
            let path = entry
                .path()
                .canonicalize()
                .map_err(|err| format!("failed to resolve search path: {err}"))?;
            if !self.path_allowed(&path)? {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
            if file_type.is_dir() {
                self.collect_search_files(&path, files)?;
            } else if file_type.is_file() {
                files.push(path);
            }
        }

        Ok(())
    }

    fn search_file(
        &self,
        path: &Path,
        options: &SearchOptions<'_>,
        results: &mut Vec<Value>,
        truncated: &mut bool,
    ) -> Result<(), String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if options.fail_on_read_error => {
                return Err(format!(
                    "failed to read {} as UTF-8 text: {err}",
                    self.display_path(path)
                ));
            }
            Err(_) => return Ok(()),
        };
        let needle = if options.case_sensitive {
            options.query.to_string()
        } else {
            options.query.to_lowercase()
        };

        for (index, line) in content.lines().enumerate() {
            let haystack = if options.case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };
            if haystack.contains(&needle) {
                if results.len() >= options.max_results {
                    *truncated = true;
                    return Ok(());
                }
                results.push(json!({
                    "path": self.display_path(path),
                    "line": index + 1,
                    "text": line,
                }));
            }
        }

        Ok(())
    }
}

fn built_in_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file from the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "list_files",
            "List files and directories under the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "recursive": {"type": "boolean"},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_ENTRIES}
                },
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "search_text",
            "Search workspace text files for a literal string.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "case_sensitive": {"type": "boolean"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "shell_command",
            "Run a shell command in the workspace root with a timeout.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_SECS}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn parse_args<T: DeserializeOwned>(call: &ToolCall) -> Result<T, String> {
    serde_json::from_str(&call.function.arguments)
        .map_err(|err| format!("invalid arguments for tool {}: {err}", call.function.name))
}

fn clamp_limit(value: Option<usize>, default: usize, max: usize) -> Result<usize, String> {
    let value = value.unwrap_or(default).min(max);
    if value == 0 {
        return Err("limit must be at least 1".to_string());
    }
    Ok(value)
}

fn should_skip_entry(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | "target")
    )
}

fn complete_tool_result(result: Result<Value, String>) -> ToolExecution {
    match result {
        Ok(data) => ToolExecution::Completed(tool_ok(data)),
        Err(error) => ToolExecution::Completed(tool_error(error)),
    }
}

fn tool_ok(data: Value) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": true,
        "data": data,
    }))
    .expect("tool result JSON must serialize");
    ToolResult {
        ok: true,
        content,
        error: None,
    }
}

fn tool_error(error: String) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": false,
        "error": error,
    }))
    .expect("tool error JSON must serialize");
    ToolResult {
        ok: false,
        error: Some(error),
        content,
    }
}

fn run_shell_command(root: &Path, command: &str, timeout: Duration) -> Result<Value, String> {
    let mut child = shell_command(command)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn shell command: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture command stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture command stderr".to_string())?;
    let stdout_reader = thread::spawn(move || read_limited(stdout));
    let stderr_reader = thread::spawn(move || read_limited(stderr));
    let started = Instant::now();
    let mut timed_out = false;

    let status = loop {
        match child
            .try_wait()
            .map_err(|err| format!("failed to wait for command: {err}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                timed_out = true;
                let _ = child.kill();
                break child
                    .wait()
                    .map_err(|err| format!("failed to wait for killed command: {err}"))?;
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    };

    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| "failed to join stdout reader".to_string())??;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| "failed to join stderr reader".to_string())??;

    Ok(json!({
        "command": command,
        "exit_code": status.code(),
        "timed_out": timed_out,
        "stdout": stdout,
        "stderr": stderr,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    }))
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut builder = Command::new("cmd");
    builder.arg("/C").arg(command);
    builder
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut builder = Command::new("sh");
    builder.arg("-c").arg(command);
    builder
}

fn read_limited(mut reader: impl Read) -> Result<(String, bool), String> {
    let mut buffer = [0_u8; 8192];
    let mut output = Vec::new();
    let mut truncated = false;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| format!("failed to read process output: {err}"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_SHELL_OUTPUT_BYTES.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }

    Ok((String::from_utf8_lossy(&output).to_string(), truncated))
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    max_lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ListFilesArgs {
    path: Option<String>,
    recursive: Option<bool>,
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SearchTextArgs {
    query: String,
    path: Option<String>,
    case_sensitive: Option<bool>,
    max_results: Option<usize>,
}

struct SearchOptions<'a> {
    query: &'a str,
    case_sensitive: bool,
    max_results: usize,
    fail_on_read_error: bool,
}

#[derive(Debug, Deserialize)]
struct ShellCommandArgs {
    command: String,
    timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{PermissionMode, ShellPolicy};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-tools-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn registry(root: &Path) -> ToolRegistry {
        ToolRegistry::built_in(
            root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        )
        .expect("tool registry")
    }

    fn registry_with_permissions(root: &Path, permissions: PermissionProfile) -> ToolRegistry {
        ToolRegistry::built_in(root, permissions).expect("tool registry")
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::function("call_1", name, arguments.to_string())
    }

    fn content(execution: ToolExecution) -> Value {
        let ToolExecution::Completed(result) = execution else {
            panic!("expected completed tool execution");
        };
        serde_json::from_str(&result.content).expect("tool JSON")
    }

    fn approval_request(execution: ToolExecution) -> ApprovalRequest {
        let ToolExecution::ApprovalRequired(request) = execution else {
            panic!("expected approval request");
        };
        request
    }

    #[test]
    fn read_file_limits_lines_and_rejects_path_escape() {
        let root = unique_dir("read-root");
        fs::write(root.join("note.txt"), "a\nb\nc\nd\n").expect("write file");
        let outside = root
            .parent()
            .expect("parent")
            .join("outside-morrow-tools.txt");
        fs::write(&outside, "secret").expect("write outside");
        let tools = registry(&root);

        let value = content(tools.execute(&call(
            "read_file",
            json!({"path": "note.txt", "start_line": 2, "max_lines": 2}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["content"], "b\nc");
        assert_eq!(value["data"]["start_line"], 2);
        assert_eq!(value["data"]["end_line"], 3);
        assert_eq!(value["data"]["truncated"], true);

        let escaped = content(tools.execute(&call(
            "read_file",
            json!({"path": outside.display().to_string()}),
        )));

        assert_eq!(escaped["ok"], false);
        assert!(
            escaped["error"]
                .as_str()
                .expect("error")
                .contains("outside the workspace root")
        );
    }

    #[test]
    fn list_files_skips_git_and_target() {
        let root = unique_dir("list-root");
        fs::write(root.join("a.txt"), "").expect("write file");
        fs::create_dir(root.join(".git")).expect("create git");
        fs::create_dir(root.join("target")).expect("create target");
        fs::create_dir(root.join("src")).expect("create src");
        fs::write(root.join("src").join("lib.rs"), "").expect("write lib");
        let tools = registry(&root);

        let value =
            content(tools.execute(&call("list_files", json!({"path": ".", "recursive": true}))));

        assert_eq!(value["ok"], true);
        let entries = value["data"]["entries"].as_array().expect("entries");
        let paths = entries
            .iter()
            .map(|entry| entry["path"].as_str().expect("path"))
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["a.txt", "src", "src/lib.rs"]);
    }

    #[test]
    fn search_text_finds_literal_matches_with_limit() {
        let root = unique_dir("search-root");
        fs::write(root.join("a.txt"), "Alpha\nbeta\nalpha\n").expect("write file");
        let tools = registry(&root);

        let value = content(tools.execute(&call(
            "search_text",
            json!({"query": "alpha", "path": ".", "max_results": 1}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["truncated"], true);
        let results = value["data"]["results"].as_array().expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["path"], "a.txt");
        assert_eq!(results[0]["line"], 1);
    }

    #[test]
    fn shell_command_runs_in_workspace_and_reports_exit_code() {
        let root = unique_dir("shell-root");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let value = content(tools.execute(&call(
            "shell_command",
            json!({"command": "pwd && exit 7", "timeout_secs": 5}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["exit_code"], 7);
        assert_eq!(value["data"]["timed_out"], false);
        assert_eq!(
            value["data"]["stdout"].as_str().expect("stdout").trim(),
            root.canonicalize()
                .expect("canonical root")
                .display()
                .to_string()
        );
    }

    #[test]
    fn shell_command_times_out() {
        let root = unique_dir("timeout-root");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let value = content(tools.execute(&call(
            "shell_command",
            json!({"command": "sleep 2", "timeout_secs": 1}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["timed_out"], true);
    }

    #[test]
    fn shell_command_requires_approval_in_workspace_write() {
        let root = unique_dir("shell-approval-root");
        let tools = registry(&root);

        let request = approval_request(tools.execute(&call(
            "shell_command",
            json!({"command": "pwd", "timeout_secs": 5}),
        )));

        assert_eq!(request.id, "approval-call_1");
    }

    #[test]
    fn shell_command_runs_after_matching_approval() {
        let root = unique_dir("shell-approved-root");
        let tools = registry(&root);
        let call = call(
            "shell_command",
            json!({"command": "pwd && exit 3", "timeout_secs": 5}),
        );
        let request = approval_request(tools.execute(&call));

        let value = content(tools.execute_approved(&call, &ApprovalDecision::approve(request.id)));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["exit_code"], 3);
    }

    #[test]
    fn shell_command_rejects_denied_approval() {
        let root = unique_dir("shell-denied-root");
        let tools = registry(&root);
        let call = call(
            "shell_command",
            json!({"command": "pwd", "timeout_secs": 5}),
        );
        let request = approval_request(tools.execute(&call));

        let value = content(tools.execute_approved(&call, &ApprovalDecision::deny(request.id)));

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "shell command approval denied");
    }

    #[test]
    fn shell_command_can_be_denied_by_policy() {
        let root = unique_dir("shell-policy-denied-root");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Deny,
            },
        );

        let value = content(tools.execute(&call(
            "shell_command",
            json!({"command": "pwd", "timeout_secs": 5}),
        )));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("shell commands are denied")
        );
    }

    #[test]
    fn danger_full_access_can_read_absolute_paths_outside_workspace() {
        let root = unique_dir("danger-read-root");
        let outside = root
            .parent()
            .expect("parent")
            .join("outside-morrow-tools-danger.txt");
        fs::write(&outside, "secret").expect("write outside");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let value = content(tools.execute(&call(
            "read_file",
            json!({"path": outside.display().to_string()}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["content"], "secret");
    }
}
