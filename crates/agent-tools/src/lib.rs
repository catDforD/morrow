use agent_protocol::{
    ApprovalDecision, ApprovalRequest, FileChangeOperation, FileChangeSummary, PermissionProfile,
    ShellCommandSummary, ToolCall, ToolDefinition, ToolExecutionSummary,
};
use agent_sandbox::{PermissionDecision, PermissionEvaluator, PermissionEvaluatorError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 1000;
const DEFAULT_LIST_ENTRIES: usize = 100;
const MAX_LIST_ENTRIES: usize = 500;
const DEFAULT_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_LINE_CHARS: usize = 500;
const MAX_SEARCH_TOTAL_BYTES: usize = 20_000;
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 30;
const MAX_SHELL_TIMEOUT_SECS: u64 = 120;
const MAX_SHELL_OUTPUT_BYTES: usize = 20_000;
const MAX_FILE_DIFF_LINES: usize = 240;
const MAX_FILE_DIFF_BYTES: usize = 20_000;
const SEARCH_SKIP_NAMES: &[&str] = &[".git", "node_modules", "dist", "build", "target"];

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
    pub summary: Option<ToolExecutionSummary>,
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

    pub fn execute_approved(
        &self,
        call: &ToolCall,
        decision: &ApprovalDecision,
        request: &ApprovalRequest,
    ) -> ToolExecution {
        self.execute_inner(call, Some((decision, request)))
    }

    fn execute_inner(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
        let result = match call.function.name.as_str() {
            "read_file" => self.read_file(call).map(tool_ok),
            "list_files" => self.list_files(call).map(tool_ok),
            "search_text" => self.search_text(call).map(tool_ok),
            "edit_file" => return self.edit_file(call, approval),
            "write_file" => return self.write_file(call, approval),
            "apply_patch" => return self.apply_patch(call, approval),
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
        let options = SearchOptions {
            query: &args.query,
            case_sensitive,
            max_results,
        };

        if let Some(ripgrep) = ripgrep_binary() {
            match self.search_text_with_ripgrep(&ripgrep, &path, &options) {
                Ok(output) => return Ok(output.into_value()),
                Err(RipgrepSearchError::Unavailable) => {}
                Err(RipgrepSearchError::Failed(error)) => return Err(error),
            }
        }

        Ok(self.search_text_fallback(&path, &options)?.into_value())
    }

    fn search_text_fallback(
        &self,
        path: &Path,
        options: &SearchOptions<'_>,
    ) -> Result<SearchOutput, String> {
        let mut output = SearchOutput::new(
            options.query,
            self.display_path(path),
            options.case_sensitive,
            options.max_results,
        );

        if path.is_file() {
            self.search_file(path, options, true, &mut output)?;
        } else if path.is_dir() {
            let mut files = Vec::new();
            self.collect_search_files(path, &mut files)?;
            for file in files {
                self.search_file(&file, options, false, &mut output)?;
                if output.result_truncated {
                    break;
                }
            }
        } else {
            return Err(format!("{} is not searchable", self.display_path(path)));
        }

        Ok(output)
    }

    fn search_text_with_ripgrep(
        &self,
        ripgrep: &Path,
        path: &Path,
        options: &SearchOptions<'_>,
    ) -> Result<SearchOutput, RipgrepSearchError> {
        let evaluator = self.evaluator().map_err(RipgrepSearchError::Failed)?;
        let search_path = self.display_path(path);
        let mut output = SearchOutput::new(
            options.query,
            search_path.clone(),
            options.case_sensitive,
            options.max_results,
        );
        let mut command = Command::new(ripgrep);
        command
            .current_dir(evaluator.root())
            .arg("--json")
            .arg("--fixed-strings")
            .arg("--color")
            .arg("never")
            .arg("--no-messages");
        if !options.case_sensitive {
            command.arg("--ignore-case");
        }
        for skipped in SEARCH_SKIP_NAMES {
            command.arg("--glob").arg(format!("!**/{skipped}/**"));
            command.arg("--glob").arg(format!("!{skipped}/**"));
        }
        command
            .arg(options.query)
            .arg(search_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RipgrepSearchError::Unavailable
            } else {
                RipgrepSearchError::Failed(format!("failed to start ripgrep: {err}"))
            }
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RipgrepSearchError::Failed("failed to capture ripgrep stdout".to_string())
        })?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut stopped_early = false;

        loop {
            line.clear();
            let read = reader.read_line(&mut line).map_err(|err| {
                RipgrepSearchError::Failed(format!("failed to read ripgrep output: {err}"))
            })?;
            if read == 0 {
                break;
            }
            let frame = line.trim_end_matches(['\r', '\n']);
            if let Some(match_event) = parse_ripgrep_match(frame)? {
                let match_path = Path::new(&match_event.path);
                let display_path = if match_path.is_absolute() {
                    self.display_path(match_path)
                } else {
                    self.display_path(&evaluator.root().join(match_path))
                };
                if !output.push_match(display_path, match_event.line, match_event.text) {
                    stopped_early = true;
                    let _ = child.kill();
                    break;
                }
            }
        }

        let status = child.wait().map_err(|err| {
            RipgrepSearchError::Failed(format!("failed to wait for ripgrep: {err}"))
        })?;
        if !stopped_early && !matches!(status.code(), Some(0 | 1)) {
            return Err(RipgrepSearchError::Failed(format!(
                "ripgrep search failed with status {status}"
            )));
        }

        Ok(output)
    }

    fn edit_file(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_edit_file(call), approval)
    }

    fn plan_edit_file(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<EditFileArgs>(call)?;
        if args.old_text.is_empty() {
            return Err("old_text must not be empty".to_string());
        }

        let path = self.resolve_write_path(&args.path)?;
        let metadata = fs::metadata(&path)
            .map_err(|err| format!("failed to inspect {}: {err}", self.display_path(&path)))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", self.display_path(&path)));
        }

        let content = fs::read_to_string(&path).map_err(|err| {
            format!(
                "failed to read {} as UTF-8 text: {err}",
                self.display_path(&path)
            )
        })?;
        let replacements = content.matches(&args.old_text).count();
        if replacements != 1 {
            return Err(format!(
                "old_text must match exactly once in {}; found {replacements}",
                self.display_path(&path)
            ));
        }

        let updated = content.replacen(&args.old_text, &args.new_text, 1);
        let display_path = self.display_path(&path);
        let summary = FileChangeSummary {
            path: display_path.clone(),
            operation: FileChangeOperation::Update,
            replacements: 1,
            created: false,
            overwritten: true,
            deleted: false,
        };
        let change = StagedPatchChange::write(
            path,
            PatchOperationKind::Update,
            updated.clone(),
            Some(metadata.permissions()),
            summary,
            Some(content),
            Some(updated),
        );
        let data = json!({
            "path": display_path,
            "replacements": 1,
            "created": false,
            "overwritten": true,
        });

        self.file_change_plan(vec![change], data)
    }

    fn write_file(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_write_file(call), approval)
    }

    fn plan_write_file(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<WriteFileArgs>(call)?;
        let overwrite = args.overwrite.unwrap_or(false);
        let path = self.resolve_write_path(&args.path)?;
        let existing = match fs::metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(format!(
                    "failed to inspect {}: {err}",
                    self.display_path(&path)
                ));
            }
        };

        if let Some(metadata) = existing.as_ref() {
            if !metadata.is_file() {
                return Err(format!("{} is not a file", self.display_path(&path)));
            }
            if !overwrite {
                return Err(format!(
                    "{} already exists; set overwrite to true to replace it",
                    self.display_path(&path)
                ));
            }
        }

        let created = existing.is_none();
        let overwritten = existing.is_some();
        let original = if overwritten {
            Some(fs::read_to_string(&path).map_err(|err| {
                format!(
                    "failed to read {} as UTF-8 text: {err}",
                    self.display_path(&path)
                )
            })?)
        } else {
            None
        };
        let permissions = existing.map(|metadata| metadata.permissions());
        let display_path = self.display_path(&path);
        let summary = FileChangeSummary {
            path: display_path.clone(),
            operation: if created {
                FileChangeOperation::Add
            } else {
                FileChangeOperation::Update
            },
            replacements: 0,
            created,
            overwritten,
            deleted: false,
        };
        let change = StagedPatchChange::write(
            path,
            if created {
                PatchOperationKind::Add
            } else {
                PatchOperationKind::Update
            },
            args.content.clone(),
            permissions,
            summary,
            original,
            Some(args.content),
        );

        let data = json!({
            "path": display_path,
            "replacements": 0,
            "created": created,
            "overwritten": overwritten,
        });

        self.file_change_plan(vec![change], data)
    }

    fn apply_patch(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
        self.execute_file_change_plan(call, self.plan_apply_patch(call), approval)
    }

    fn plan_apply_patch(&self, call: &ToolCall) -> Result<FileChangePlan, String> {
        let args = parse_args::<ApplyPatchArgs>(call)?;
        let operations = parse_patch(&args.patch)?;
        let changes = self.plan_patch_changes(operations)?;
        let files = changes
            .iter()
            .map(|change| file_change_summary_json(&change.summary))
            .collect::<Vec<_>>();
        let data = json!({
            "changed_files": files.len(),
            "files": files,
        });

        self.file_change_plan(changes, data)
    }

    fn execute_file_change_plan(
        &self,
        call: &ToolCall,
        plan: Result<FileChangePlan, String>,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => return ToolExecution::error(error),
        };
        let evaluator = match self.evaluator() {
            Ok(evaluator) => evaluator,
            Err(error) => return ToolExecution::error(error),
        };

        match evaluator.file_changes_decision(&call.id, plan.files.clone(), plan.diff.clone()) {
            PermissionDecision::Allow => self.commit_file_change_plan(plan),
            PermissionDecision::Deny(error) => ToolExecution::error(error),
            PermissionDecision::Prompt(request) => match approval {
                None => ToolExecution::ApprovalRequired(request),
                Some((decision, original_request)) => {
                    if decision.request_id != original_request.id {
                        return ToolExecution::error(format!(
                            "approval decision {} does not match pending approval {}",
                            decision.request_id, original_request.id
                        ));
                    }
                    if original_request.id != request.id {
                        return ToolExecution::error(format!(
                            "approval request {} does not match required approval {}",
                            original_request.id, request.id
                        ));
                    }
                    if original_request.action != request.action {
                        return ToolExecution::error(
                            "file changes changed since approval request; approval no longer matches planned changes",
                        );
                    }
                    if !decision.approved {
                        return ToolExecution::error("file changes approval denied");
                    }
                    self.commit_file_change_plan(plan)
                }
            },
        }
    }

    fn commit_file_change_plan(&self, plan: FileChangePlan) -> ToolExecution {
        match commit_patch_changes(plan.changes, self) {
            Ok(()) => ToolExecution::Completed(tool_ok_with_summary(plan.data, plan.summary)),
            Err(error) => ToolExecution::error(error),
        }
    }

    fn file_change_plan(
        &self,
        changes: Vec<StagedPatchChange>,
        data: Value,
    ) -> Result<FileChangePlan, String> {
        let files = changes
            .iter()
            .map(|change| change.summary.clone())
            .collect::<Vec<_>>();
        let diff = render_file_diff(&changes, self);
        let summary = ToolExecutionSummary::file_changes(files.clone(), diff.clone());

        Ok(FileChangePlan {
            changes,
            data,
            files,
            diff,
            summary,
        })
    }

    fn shell_command(
        &self,
        call: &ToolCall,
        approval: Option<(&ApprovalDecision, &ApprovalRequest)>,
    ) -> ToolExecution {
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
            PermissionDecision::Allow => complete_shell_result(run_shell_command(
                evaluator.root(),
                &args.command,
                Duration::from_secs(timeout_secs),
            )),
            PermissionDecision::Deny(error) => ToolExecution::error(error),
            PermissionDecision::Prompt(request) => match approval {
                None => ToolExecution::ApprovalRequired(request),
                Some((decision, _)) if decision.request_id != request.id => {
                    ToolExecution::error(format!(
                        "approval decision {} does not match required approval {}",
                        decision.request_id, request.id
                    ))
                }
                Some((decision, _)) if !decision.approved => {
                    ToolExecution::error("shell command approval denied")
                }
                Some(_) => complete_shell_result(run_shell_command(
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

    fn resolve_write_path(&self, input: &str) -> Result<PathBuf, String> {
        self.evaluator()?.resolve_write_path(input)
    }

    fn plan_patch_changes(
        &self,
        operations: Vec<ParsedPatchOperation>,
    ) -> Result<Vec<StagedPatchChange>, String> {
        let mut paths = HashSet::new();
        let mut changes = Vec::with_capacity(operations.len());

        for operation in operations {
            let path = self.resolve_write_path(operation.path())?;
            if !paths.insert(path.clone()) {
                return Err(format!(
                    "patch modifies {} more than once",
                    self.display_path(&path)
                ));
            }

            let change = match operation {
                ParsedPatchOperation::Add { path: _, content } => {
                    match fs::metadata(&path) {
                        Ok(_) => {
                            return Err(format!(
                                "{} already exists; add file cannot overwrite it",
                                self.display_path(&path)
                            ));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(format!(
                                "failed to inspect {}: {err}",
                                self.display_path(&path)
                            ));
                        }
                    }
                    let summary = FileChangeSummary {
                        path: self.display_path(&path),
                        operation: FileChangeOperation::Add,
                        replacements: 0,
                        created: true,
                        overwritten: false,
                        deleted: false,
                    };
                    StagedPatchChange::write(
                        path.clone(),
                        PatchOperationKind::Add,
                        content.clone(),
                        None,
                        summary,
                        None,
                        Some(content),
                    )
                }
                ParsedPatchOperation::Update { path: _, hunks } => {
                    let metadata = fs::metadata(&path).map_err(|err| {
                        format!("failed to inspect {}: {err}", self.display_path(&path))
                    })?;
                    if !metadata.is_file() {
                        return Err(format!("{} is not a file", self.display_path(&path)));
                    }

                    let original = fs::read_to_string(&path).map_err(|err| {
                        format!(
                            "failed to read {} as UTF-8 text: {err}",
                            self.display_path(&path)
                        )
                    })?;
                    let mut updated = original.clone();
                    let mut replacements = 0;
                    for hunk in hunks {
                        let matches = updated.matches(&hunk.old_text).count();
                        if matches != 1 {
                            return Err(format!(
                                "patch hunk for {} must match exactly once; found {matches}",
                                self.display_path(&path)
                            ));
                        }
                        updated = updated.replacen(&hunk.old_text, &hunk.new_text, 1);
                        replacements += 1;
                    }
                    if updated == original {
                        return Err(format!(
                            "patch update for {} did not change file content",
                            self.display_path(&path)
                        ));
                    }

                    StagedPatchChange::write(
                        path.clone(),
                        PatchOperationKind::Update,
                        updated.clone(),
                        Some(metadata.permissions()),
                        FileChangeSummary {
                            path: self.display_path(&path),
                            operation: FileChangeOperation::Update,
                            replacements,
                            created: false,
                            overwritten: true,
                            deleted: false,
                        },
                        Some(original),
                        Some(updated),
                    )
                }
                ParsedPatchOperation::Delete { path: _ } => {
                    let metadata = fs::metadata(&path).map_err(|err| {
                        format!("failed to inspect {}: {err}", self.display_path(&path))
                    })?;
                    if !metadata.is_file() {
                        return Err(format!("{} is not a file", self.display_path(&path)));
                    }
                    let original = fs::read_to_string(&path).map_err(|err| {
                        format!(
                            "failed to read {} as UTF-8 text: {err}",
                            self.display_path(&path)
                        )
                    })?;

                    StagedPatchChange::delete(
                        path.clone(),
                        FileChangeSummary {
                            path: self.display_path(&path),
                            operation: FileChangeOperation::Delete,
                            replacements: 0,
                            created: false,
                            overwritten: false,
                            deleted: true,
                        },
                        Some(original),
                        None,
                    )
                }
            };
            changes.push(change);
        }

        Ok(changes)
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
        fail_on_read_error: bool,
        output: &mut SearchOutput,
    ) -> Result<(), String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if fail_on_read_error => {
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
            if haystack.contains(&needle)
                && !output.push_match(self.display_path(path), index + 1, line.to_string())
            {
                return Ok(());
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
            "edit_file",
            "Edit a UTF-8 text file by replacing text that matches exactly once.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string", "minLength": 1},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "write_file",
            "Create or overwrite a UTF-8 text file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "overwrite": {"type": "boolean"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::function(
            "apply_patch",
            "Apply a patch to add, update, or delete files.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Patch text to apply."
                    }
                },
                "required": ["patch"],
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
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SEARCH_SKIP_NAMES.contains(&name))
}

#[cfg(windows)]
fn ripgrep_sidecar_name() -> &'static str {
    "morrow-rg.exe"
}

#[cfg(not(windows))]
fn ripgrep_sidecar_name() -> &'static str {
    "morrow-rg"
}

#[cfg(windows)]
fn path_ripgrep_name() -> &'static str {
    "rg.exe"
}

#[cfg(not(windows))]
fn path_ripgrep_name() -> &'static str {
    "rg"
}

fn ripgrep_binary() -> Option<PathBuf> {
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let sidecar = dir.join(ripgrep_sidecar_name());
        if sidecar.is_file() {
            return Some(sidecar);
        }
    }

    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(path_ripgrep_name()))
            .find(|candidate| candidate.is_file())
    })
}

#[derive(Debug)]
enum RipgrepSearchError {
    Unavailable,
    Failed(String),
}

struct SearchOutput {
    query: String,
    path: String,
    case_sensitive: bool,
    max_results: usize,
    total_result_bytes: usize,
    result_truncated: bool,
    results: Vec<Value>,
}

impl SearchOutput {
    fn new(
        query: impl Into<String>,
        path: impl Into<String>,
        case_sensitive: bool,
        max_results: usize,
    ) -> Self {
        Self {
            query: query.into(),
            path: path.into(),
            case_sensitive,
            max_results,
            total_result_bytes: 0,
            result_truncated: false,
            results: Vec::new(),
        }
    }

    fn push_match(&mut self, path: String, line: usize, text: String) -> bool {
        if self.results.len() >= self.max_results {
            self.result_truncated = true;
            return false;
        }

        let (text, text_truncated) = truncate_chars(trim_line_endings(text), MAX_SEARCH_LINE_CHARS);
        let item = json!({
            "path": path,
            "line": line,
            "text": text,
            "text_truncated": text_truncated,
        });
        let item_bytes = serde_json::to_vec(&item)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if self.total_result_bytes.saturating_add(item_bytes) > MAX_SEARCH_TOTAL_BYTES {
            self.result_truncated = true;
            return false;
        }

        self.total_result_bytes += item_bytes;
        self.results.push(item);
        true
    }

    fn into_value(self) -> Value {
        json!({
            "query": self.query,
            "path": self.path,
            "case_sensitive": self.case_sensitive,
            "truncated": self.result_truncated,
            "result_truncated": self.result_truncated,
            "results": self.results,
        })
    }
}

fn trim_line_endings(mut text: String) -> String {
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}

fn truncate_chars(text: String, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = chars.clone().nth(max_chars).is_some();
    if !truncated {
        return (text, false);
    }
    (chars.by_ref().take(max_chars).collect(), true)
}

#[derive(Debug, Deserialize)]
struct RipgrepEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RipgrepEventData>,
}

#[derive(Debug, Deserialize)]
struct RipgrepEventData {
    path: Option<RipgrepText>,
    lines: Option<RipgrepText>,
    line_number: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RipgrepText {
    text: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct RipgrepMatch {
    path: String,
    line: usize,
    text: String,
}

fn parse_ripgrep_match(frame: &str) -> Result<Option<RipgrepMatch>, RipgrepSearchError> {
    if frame.trim().is_empty() {
        return Ok(None);
    }
    let event = serde_json::from_str::<RipgrepEvent>(frame).map_err(|err| {
        RipgrepSearchError::Failed(format!("failed to parse ripgrep JSON output: {err}"))
    })?;
    if event.kind != "match" {
        return Ok(None);
    }
    let Some(data) = event.data else {
        return Ok(None);
    };
    let Some(path) = data.path.and_then(|path| path.text) else {
        return Ok(None);
    };
    let Some(text) = data.lines.and_then(|lines| lines.text) else {
        return Ok(None);
    };
    let Some(line) = data.line_number else {
        return Ok(None);
    };

    Ok(Some(RipgrepMatch { path, line, text }))
}

fn file_change_summary_json(summary: &FileChangeSummary) -> Value {
    json!({
        "path": summary.path,
        "operation": summary.operation.as_str(),
        "replacements": summary.replacements,
        "created": summary.created,
        "overwritten": summary.overwritten,
        "deleted": summary.deleted,
    })
}

fn render_file_diff(changes: &[StagedPatchChange], tools: &ToolRegistry) -> String {
    let mut builder = DiffBuilder::default();

    for change in changes {
        let path = tools.display_path(&change.path);
        let old_path = if matches!(change.kind, PatchOperationKind::Add) {
            "/dev/null"
        } else {
            path.as_str()
        };
        let new_path = if matches!(change.kind, PatchOperationKind::Delete) {
            "/dev/null"
        } else {
            path.as_str()
        };
        builder.push_line(&format!("--- {old_path}"));
        builder.push_line(&format!("+++ {new_path}"));
        builder.push_line("@@");
        if let Some(before) = change.before.as_deref() {
            for line in before.lines() {
                builder.push_line(&format!("-{line}"));
            }
        }
        if let Some(after) = change.after.as_deref() {
            for line in after.lines() {
                builder.push_line(&format!("+{line}"));
            }
        }
        builder.push_line("");
    }

    builder.finish()
}

#[derive(Default)]
struct DiffBuilder {
    output: String,
    lines: usize,
    truncated: bool,
}

impl DiffBuilder {
    fn push_line(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        if self.lines >= MAX_FILE_DIFF_LINES
            || self
                .output
                .len()
                .saturating_add(line.len())
                .saturating_add(1)
                > MAX_FILE_DIFF_BYTES
        {
            self.truncated = true;
            return;
        }
        self.output.push_str(line);
        self.output.push('\n');
        self.lines += 1;
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.output.push_str("... diff truncated ...\n");
        }
        self.output
    }
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "morrow-write".into());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_file_name(format!(".{file_name}.tmp-{}-{stamp}", std::process::id()))
}

fn backup_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "morrow-backup".into());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_file_name(format!(".{file_name}.bak-{}-{stamp}", std::process::id()))
}

fn write_temp_file(
    display_path: &Path,
    temp_path: &Path,
    content: &str,
    permissions: Option<fs::Permissions>,
    tools: &ToolRegistry,
) -> Result<(), String> {
    fs::write(temp_path, content).map_err(|err| {
        format!(
            "failed to write temporary file for {}: {err}",
            tools.display_path(display_path)
        )
    })?;
    if let Some(permissions) = permissions {
        fs::set_permissions(temp_path, permissions).map_err(|err| {
            let _ = fs::remove_file(temp_path);
            format!(
                "failed to set permissions on temporary file for {}: {err}",
                tools.display_path(display_path)
            )
        })?;
    }

    Ok(())
}

fn commit_patch_changes(
    mut changes: Vec<StagedPatchChange>,
    tools: &ToolRegistry,
) -> Result<(), String> {
    for index in 0..changes.len() {
        let Some(content) = changes[index].content.as_deref() else {
            continue;
        };
        let temp_path = temp_path_for(&changes[index].path);
        if let Err(error) = write_temp_file(
            &changes[index].path,
            &temp_path,
            content,
            changes[index].permissions.clone(),
            tools,
        ) {
            cleanup_patch_temps(&changes);
            return Err(error);
        }
        changes[index].temp_path = Some(temp_path);
    }

    let mut applied = Vec::new();
    for change in &mut changes {
        match change.kind {
            PatchOperationKind::Add => {
                if change.path.exists() {
                    return fail_patch_commit(
                        format!(
                            "{} already exists; add file cannot overwrite it",
                            tools.display_path(&change.path)
                        ),
                        &changes,
                        applied,
                        tools,
                    );
                }
                let temp_path = change
                    .temp_path
                    .take()
                    .ok_or_else(|| "staged add file is missing temporary content".to_string())?;
                if let Err(err) = fs::rename(&temp_path, &change.path) {
                    let _ = fs::remove_file(&temp_path);
                    return fail_patch_commit(
                        format!(
                            "failed to create {}: {err}",
                            tools.display_path(&change.path)
                        ),
                        &changes,
                        applied,
                        tools,
                    );
                }
                applied.push(AppliedPatchChange {
                    path: change.path.clone(),
                    kind: PatchOperationKind::Add,
                    backup_path: None,
                });
            }
            PatchOperationKind::Update => {
                let temp_path = change
                    .temp_path
                    .take()
                    .ok_or_else(|| "staged update file is missing temporary content".to_string())?;
                let backup_path = backup_path_for(&change.path);
                if let Err(err) = fs::rename(&change.path, &backup_path) {
                    let _ = fs::remove_file(&temp_path);
                    return fail_patch_commit(
                        format!(
                            "failed to back up {}: {err}",
                            tools.display_path(&change.path)
                        ),
                        &changes,
                        applied,
                        tools,
                    );
                }
                if let Err(err) = fs::rename(&temp_path, &change.path) {
                    let _ = fs::rename(&backup_path, &change.path);
                    let _ = fs::remove_file(&temp_path);
                    return fail_patch_commit(
                        format!(
                            "failed to replace {}: {err}",
                            tools.display_path(&change.path)
                        ),
                        &changes,
                        applied,
                        tools,
                    );
                }
                applied.push(AppliedPatchChange {
                    path: change.path.clone(),
                    kind: PatchOperationKind::Update,
                    backup_path: Some(backup_path),
                });
            }
            PatchOperationKind::Delete => {
                let backup_path = backup_path_for(&change.path);
                if let Err(err) = fs::rename(&change.path, &backup_path) {
                    return fail_patch_commit(
                        format!(
                            "failed to delete {}: {err}",
                            tools.display_path(&change.path)
                        ),
                        &changes,
                        applied,
                        tools,
                    );
                }
                applied.push(AppliedPatchChange {
                    path: change.path.clone(),
                    kind: PatchOperationKind::Delete,
                    backup_path: Some(backup_path),
                });
            }
        }
    }

    for change in applied {
        if let Some(backup_path) = change.backup_path {
            let _ = fs::remove_file(backup_path);
        }
    }

    Ok(())
}

fn cleanup_patch_temps(changes: &[StagedPatchChange]) {
    for change in changes {
        if let Some(temp_path) = change.temp_path.as_ref() {
            let _ = fs::remove_file(temp_path);
        }
    }
}

fn fail_patch_commit(
    error: String,
    changes: &[StagedPatchChange],
    applied: Vec<AppliedPatchChange>,
    tools: &ToolRegistry,
) -> Result<(), String> {
    cleanup_patch_temps(changes);
    let rollback_errors = rollback_patch_changes(applied, tools);
    if rollback_errors.is_empty() {
        Err(error)
    } else {
        Err(format!(
            "{error}; rollback errors: {}",
            rollback_errors.join("; ")
        ))
    }
}

fn rollback_patch_changes(
    mut applied: Vec<AppliedPatchChange>,
    tools: &ToolRegistry,
) -> Vec<String> {
    let mut errors = Vec::new();
    while let Some(change) = applied.pop() {
        match change.kind {
            PatchOperationKind::Add => {
                if let Err(err) = fs::remove_file(&change.path) {
                    errors.push(format!(
                        "failed to remove created {}: {err}",
                        tools.display_path(&change.path)
                    ));
                }
            }
            PatchOperationKind::Update => {
                if let Err(err) = fs::remove_file(&change.path) {
                    errors.push(format!(
                        "failed to remove updated {}: {err}",
                        tools.display_path(&change.path)
                    ));
                }
                if let Some(backup_path) = change.backup_path
                    && let Err(err) = fs::rename(&backup_path, &change.path)
                {
                    errors.push(format!(
                        "failed to restore {}: {err}",
                        tools.display_path(&change.path)
                    ));
                }
            }
            PatchOperationKind::Delete => {
                if let Some(backup_path) = change.backup_path
                    && let Err(err) = fs::rename(&backup_path, &change.path)
                {
                    errors.push(format!(
                        "failed to restore deleted {}: {err}",
                        tools.display_path(&change.path)
                    ));
                }
            }
        }
    }
    errors
}

fn parse_patch(patch: &str) -> Result<Vec<ParsedPatchOperation>, String> {
    let normalized = patch.replace("\r\n", "\n");
    let mut lines = normalized.split('\n').collect::<Vec<_>>();
    while matches!(lines.last(), Some(line) if line.is_empty()) {
        lines.pop();
    }

    if lines.first().copied() != Some("*** Begin Patch") {
        return Err("patch must start with *** Begin Patch".to_string());
    }
    if lines.last().copied() != Some("*** End Patch") {
        return Err("patch must end with *** End Patch".to_string());
    }
    if lines.len() <= 2 {
        return Err("patch must contain at least one operation".to_string());
    }

    let end = lines.len() - 1;
    let mut index = 1;
    let mut operations = Vec::new();
    while index < end {
        let line = lines[index];
        if line.starts_with("*** Move to:") {
            return Err("apply_patch does not support move operations".to_string());
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = parse_patch_path(path)?;
            index += 1;
            let mut content = String::new();
            let mut line_count = 0;
            while index < end && !is_patch_directive(lines[index]) {
                let line = lines[index];
                let Some(payload) = line.strip_prefix('+') else {
                    return Err(format!(
                        "invalid add file line for {path}; expected + prefix"
                    ));
                };
                push_patch_line(&mut content, payload);
                line_count += 1;
                index += 1;
            }
            if line_count == 0 {
                return Err(format!("add file {path} must contain at least one line"));
            }
            operations.push(ParsedPatchOperation::Add { path, content });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = parse_patch_path(path)?;
            index += 1;
            let mut hunks = Vec::new();
            while index < end && !is_patch_directive(lines[index]) {
                if !lines[index].starts_with("@@") {
                    return Err(format!("expected @@ hunk header for update file {path}"));
                }
                index += 1;
                let mut old_text = String::new();
                let mut new_text = String::new();
                let mut line_count = 0;
                while index < end
                    && !lines[index].starts_with("@@")
                    && !is_patch_directive(lines[index])
                {
                    let line = lines[index];
                    let Some(prefix) = line.chars().next() else {
                        return Err(format!("invalid empty hunk line for update file {path}"));
                    };
                    let payload = &line[prefix.len_utf8()..];
                    match prefix {
                        ' ' => {
                            push_patch_line(&mut old_text, payload);
                            push_patch_line(&mut new_text, payload);
                        }
                        '-' => push_patch_line(&mut old_text, payload),
                        '+' => push_patch_line(&mut new_text, payload),
                        _ => {
                            return Err(format!(
                                "invalid hunk line prefix {prefix:?} for update file {path}"
                            ));
                        }
                    }
                    line_count += 1;
                    index += 1;
                }
                if line_count == 0 {
                    return Err(format!("empty hunk for update file {path}"));
                }
                if old_text.is_empty() {
                    return Err(format!(
                        "hunk for update file {path} must include context or removed lines"
                    ));
                }
                if old_text == new_text {
                    return Err(format!("hunk for update file {path} has no changes"));
                }
                hunks.push(PatchHunk { old_text, new_text });
            }
            if hunks.is_empty() {
                return Err(format!("update file {path} must contain at least one hunk"));
            }
            operations.push(ParsedPatchOperation::Update { path, hunks });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            let path = parse_patch_path(path)?;
            index += 1;
            operations.push(ParsedPatchOperation::Delete { path });
            continue;
        }

        if line.starts_with("*** ") {
            return Err(format!("unknown patch operation {line:?}"));
        }
        return Err(format!("expected patch operation, found {line:?}"));
    }

    Ok(operations)
}

fn parse_patch_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("patch operation path must not be empty".to_string());
    }
    Ok(path.to_string())
}

fn is_patch_directive(line: &str) -> bool {
    line.starts_with("*** ")
}

fn push_patch_line(content: &mut String, line: &str) {
    content.push_str(line);
    content.push('\n');
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedPatchOperation {
    Add { path: String, content: String },
    Update { path: String, hunks: Vec<PatchHunk> },
    Delete { path: String },
}

impl ParsedPatchOperation {
    fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } | Self::Delete { path } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchHunk {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchOperationKind {
    Add,
    Update,
    Delete,
}

#[derive(Debug)]
struct StagedPatchChange {
    path: PathBuf,
    kind: PatchOperationKind,
    content: Option<String>,
    permissions: Option<fs::Permissions>,
    summary: FileChangeSummary,
    before: Option<String>,
    after: Option<String>,
    temp_path: Option<PathBuf>,
}

impl StagedPatchChange {
    fn write(
        path: PathBuf,
        kind: PatchOperationKind,
        content: String,
        permissions: Option<fs::Permissions>,
        summary: FileChangeSummary,
        before: Option<String>,
        after: Option<String>,
    ) -> Self {
        Self {
            path,
            kind,
            content: Some(content),
            permissions,
            summary,
            before,
            after,
            temp_path: None,
        }
    }

    fn delete(
        path: PathBuf,
        summary: FileChangeSummary,
        before: Option<String>,
        after: Option<String>,
    ) -> Self {
        Self {
            path,
            kind: PatchOperationKind::Delete,
            content: None,
            permissions: None,
            summary,
            before,
            after,
            temp_path: None,
        }
    }
}

#[derive(Debug)]
struct FileChangePlan {
    changes: Vec<StagedPatchChange>,
    data: Value,
    files: Vec<FileChangeSummary>,
    diff: String,
    summary: ToolExecutionSummary,
}

#[derive(Debug)]
struct AppliedPatchChange {
    path: PathBuf,
    kind: PatchOperationKind,
    backup_path: Option<PathBuf>,
}

fn tool_ok(data: Value) -> ToolResult {
    tool_ok_inner(data, None)
}

fn tool_ok_with_summary(data: Value, summary: ToolExecutionSummary) -> ToolResult {
    tool_ok_inner(data, Some(summary))
}

fn tool_ok_inner(data: Value, summary: Option<ToolExecutionSummary>) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": true,
        "data": data,
    }))
    .expect("tool result JSON must serialize");
    ToolResult {
        ok: true,
        content,
        error: None,
        summary,
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
        error: Some(error.clone()),
        content,
        summary: Some(ToolExecutionSummary::error(error)),
    }
}

fn complete_shell_result(result: Result<(Value, ShellCommandSummary), String>) -> ToolExecution {
    match result {
        Ok((data, summary)) => ToolExecution::Completed(tool_ok_with_summary(
            data,
            ToolExecutionSummary::shell(summary),
        )),
        Err(error) => ToolExecution::error(error),
    }
}

fn run_shell_command(
    root: &Path,
    command: &str,
    timeout: Duration,
) -> Result<(Value, ShellCommandSummary), String> {
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

    let exit_code = status.code();
    let data = json!({
        "command": command,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": stdout,
        "stderr": stderr,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    });
    let summary = ShellCommandSummary {
        command: command.to_string(),
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    };

    Ok((data, summary))
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
}

#[derive(Debug, Deserialize)]
struct EditFileArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    overwrite: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ApplyPatchArgs {
    patch: String,
}

#[derive(Debug, Deserialize)]
struct ShellCommandArgs {
    command: String,
    timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{ApprovalAction, PermissionMode, ShellPolicy};
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

    fn outside_path(root: &Path, name: &str) -> PathBuf {
        let root_name = root.file_name().expect("root file name").to_string_lossy();
        root.parent()
            .expect("root parent")
            .join(format!("{root_name}-{name}"))
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

    fn patch_call(patch: &str) -> ToolCall {
        call("apply_patch", json!({"patch": patch}))
    }

    fn content(execution: ToolExecution) -> Value {
        let result = completed_result(execution);
        serde_json::from_str(&result.content).expect("tool JSON")
    }

    fn completed_result(execution: ToolExecution) -> ToolResult {
        let ToolExecution::Completed(result) = execution else {
            panic!("expected completed tool execution");
        };
        result
    }

    fn approval_request(execution: ToolExecution) -> ApprovalRequest {
        let ToolExecution::ApprovalRequired(request) = execution else {
            panic!("expected approval request");
        };
        request
    }

    fn approved_content(tools: &ToolRegistry, call: &ToolCall) -> Value {
        let request = approval_request(tools.execute(call));
        assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
        content(tools.execute_approved(
            call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ))
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
        assert_eq!(value["data"]["result_truncated"], true);
        let results = value["data"]["results"].as_array().expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["path"], "a.txt");
        assert_eq!(results[0]["line"], 1);
        assert_eq!(results[0]["text_truncated"], false);
    }

    #[test]
    fn ripgrep_json_parser_reads_match_events_only() {
        let match_frame = json!({
            "type": "match",
            "data": {
                "path": {"text": "src/lib.rs"},
                "lines": {"text": "robot doctor\n"},
                "line_number": 42
            }
        })
        .to_string();
        let begin_frame = json!({
            "type": "begin",
            "data": {"path": {"text": "src/lib.rs"}}
        })
        .to_string();

        assert_eq!(parse_ripgrep_match(&begin_frame).expect("begin"), None);
        assert_eq!(
            parse_ripgrep_match(&match_frame).expect("match"),
            Some(RipgrepMatch {
                path: "src/lib.rs".to_string(),
                line: 42,
                text: "robot doctor\n".to_string(),
            })
        );
    }

    #[test]
    fn search_output_truncates_long_lines() {
        let mut output = SearchOutput::new("needle", ".", false, 10);
        assert!(output.push_match(
            "long.txt".to_string(),
            1,
            format!("needle {}", "x".repeat(MAX_SEARCH_LINE_CHARS + 20)),
        ));

        let value = output.into_value();
        let result = &value["results"][0];
        assert_eq!(value["result_truncated"], false);
        assert_eq!(result["text_truncated"], true);
        assert_eq!(
            result["text"].as_str().expect("text").chars().count(),
            MAX_SEARCH_LINE_CHARS
        );
    }

    #[test]
    fn search_output_marks_result_truncation_for_limits() {
        let mut output = SearchOutput::new("needle", ".", false, 1);
        assert!(output.push_match("a.txt".to_string(), 1, "needle".to_string()));
        assert!(!output.push_match("b.txt".to_string(), 1, "needle".to_string()));

        let value = output.into_value();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["result_truncated"], true);
        assert_eq!(value["results"].as_array().expect("results").len(), 1);
    }

    #[test]
    fn search_output_marks_result_truncation_for_total_budget() {
        let mut output = SearchOutput::new("needle", ".", false, MAX_SEARCH_RESULTS);
        let long = format!("needle {}", "x".repeat(MAX_SEARCH_LINE_CHARS));

        while output.push_match("budget.txt".to_string(), 1, long.clone()) {}

        let value = output.into_value();
        assert_eq!(value["result_truncated"], true);
        assert!(
            value["results"].as_array().expect("results").len() < MAX_SEARCH_RESULTS,
            "total byte budget should truncate before max_results"
        );
    }

    #[test]
    fn search_text_respects_case_sensitivity() {
        let root = unique_dir("search-case-root");
        fs::write(root.join("a.txt"), "Alpha\n").expect("write file");
        let tools = registry(&root);

        let insensitive = content(tools.execute(&call(
            "search_text",
            json!({"query": "alpha", "path": ".", "case_sensitive": false}),
        )));
        let sensitive = content(tools.execute(&call(
            "search_text",
            json!({"query": "alpha", "path": ".", "case_sensitive": true}),
        )));

        assert_eq!(
            insensitive["data"]["results"]
                .as_array()
                .expect("insensitive results")
                .len(),
            1
        );
        assert_eq!(
            sensitive["data"]["results"]
                .as_array()
                .expect("sensitive results")
                .len(),
            0
        );
    }

    #[test]
    fn search_text_skips_generated_directories() {
        let root = unique_dir("search-skip-root");
        fs::write(root.join("keep.txt"), "needle\n").expect("write keep");
        for dir in SEARCH_SKIP_NAMES {
            let skipped = root.join(dir);
            fs::create_dir_all(&skipped).expect("create skipped dir");
            fs::write(skipped.join("skip.txt"), "needle\n").expect("write skipped");
        }
        let tools = registry(&root);

        let value = content(tools.execute(&call(
            "search_text",
            json!({"query": "needle", "path": ".", "max_results": 10}),
        )));

        let paths = value["data"]["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|result| result["path"].as_str().expect("path"))
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["keep.txt"]);
    }

    #[test]
    fn fallback_search_applies_output_budget() {
        let root = unique_dir("search-fallback-root");
        let path = root.join("long.txt");
        fs::write(
            &path,
            format!("needle {}\n", "x".repeat(MAX_SEARCH_LINE_CHARS + 20)),
        )
        .expect("write long file");
        let tools = registry(&root);
        let options = SearchOptions {
            query: "needle",
            case_sensitive: false,
            max_results: 10,
        };

        let output = tools
            .search_text_fallback(&path.canonicalize().expect("canonical path"), &options)
            .expect("fallback search")
            .into_value();

        assert_eq!(output["results"][0]["text_truncated"], true);
    }

    #[test]
    fn edit_file_replaces_unique_match() {
        let root = unique_dir("edit-root");
        fs::write(root.join("note.txt"), "before old after\n").expect("write file");
        let tools = registry(&root);
        let call = call(
            "edit_file",
            json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
        );

        let request = approval_request(tools.execute(&call));
        assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read before approval"),
            "before old after\n"
        );
        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["path"], "note.txt");
        assert_eq!(value["data"]["replacements"], 1);
        assert_eq!(value["data"]["created"], false);
        assert_eq!(value["data"]["overwritten"], true);
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read file"),
            "before new after\n"
        );
    }

    #[test]
    fn edit_file_rejects_invalid_matches_and_targets() {
        let root = unique_dir("edit-invalid-root");
        fs::write(root.join("no-match.txt"), "alpha\n").expect("write no match");
        fs::write(root.join("many.txt"), "alpha alpha\n").expect("write many");
        fs::create_dir(root.join("dir")).expect("create dir");
        let tools = registry(&root);

        let no_match = content(tools.execute(&call(
            "edit_file",
            json!({"path": "no-match.txt", "old_text": "beta", "new_text": "gamma"}),
        )));
        assert_eq!(no_match["ok"], false);
        assert!(
            no_match["error"]
                .as_str()
                .expect("error")
                .contains("found 0")
        );

        let many = content(tools.execute(&call(
            "edit_file",
            json!({"path": "many.txt", "old_text": "alpha", "new_text": "beta"}),
        )));
        assert_eq!(many["ok"], false);
        assert!(many["error"].as_str().expect("error").contains("found 2"));

        let empty = content(tools.execute(&call(
            "edit_file",
            json!({"path": "no-match.txt", "old_text": "", "new_text": "gamma"}),
        )));
        assert_eq!(empty["ok"], false);
        assert!(
            empty["error"]
                .as_str()
                .expect("error")
                .contains("old_text must not be empty")
        );

        let missing = content(tools.execute(&call(
            "edit_file",
            json!({"path": "missing.txt", "old_text": "a", "new_text": "b"}),
        )));
        assert_eq!(missing["ok"], false);
        assert!(
            missing["error"]
                .as_str()
                .expect("error")
                .contains("failed to inspect")
        );

        let directory = content(tools.execute(&call(
            "edit_file",
            json!({"path": "dir", "old_text": "a", "new_text": "b"}),
        )));
        assert_eq!(directory["ok"], false);
        assert!(
            directory["error"]
                .as_str()
                .expect("error")
                .contains("is not a file")
        );

        assert_eq!(
            fs::read_to_string(root.join("no-match.txt")).expect("read no match"),
            "alpha\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("many.txt")).expect("read many"),
            "alpha alpha\n"
        );
    }

    #[test]
    fn write_file_creates_new_file() {
        let root = unique_dir("write-create-root");
        let tools = registry(&root);
        let call = call(
            "write_file",
            json!({"path": "note.txt", "content": "created\n"}),
        );

        let request = approval_request(tools.execute(&call));
        assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
        assert!(!root.join("note.txt").exists());
        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["path"], "note.txt");
        assert_eq!(value["data"]["replacements"], 0);
        assert_eq!(value["data"]["created"], true);
        assert_eq!(value["data"]["overwritten"], false);
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read file"),
            "created\n"
        );
    }

    #[test]
    fn file_change_approval_returns_diff_summary() {
        let root = unique_dir("write-summary-root");
        let tools = registry(&root);
        let call = call(
            "write_file",
            json!({"path": "note.txt", "content": "created\n"}),
        );

        let request = approval_request(tools.execute(&call));
        let ApprovalAction::FileChanges { files, diff } = &request.action else {
            panic!("expected file changes approval");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].operation, FileChangeOperation::Add);
        assert!(diff.contains("+++ note.txt"));
        assert!(diff.contains("+created"));

        let result = completed_result(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));
        let summary = result.summary.as_ref().expect("summary");

        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.files[0].path, "note.txt");
        assert!(summary.diff.as_deref().expect("diff").contains("+created"));
    }

    #[test]
    fn write_file_rejects_default_overwrite_and_preserves_file() {
        let root = unique_dir("write-default-overwrite-root");
        fs::write(root.join("note.txt"), "old\n").expect("write file");
        let tools = registry(&root);

        let value = content(tools.execute(&call(
            "write_file",
            json!({"path": "note.txt", "content": "new\n"}),
        )));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("already exists")
        );
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read file"),
            "old\n"
        );
    }

    #[test]
    fn write_file_overwrites_existing_file_when_requested() {
        let root = unique_dir("write-overwrite-root");
        fs::write(root.join("note.txt"), "old\n").expect("write file");
        let tools = registry(&root);
        let call = call(
            "write_file",
            json!({"path": "note.txt", "content": "new\n", "overwrite": true}),
        );

        let request = approval_request(tools.execute(&call));
        assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read before approval"),
            "old\n"
        );
        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["created"], false);
        assert_eq!(value["data"]["overwritten"], true);
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read file"),
            "new\n"
        );
    }

    #[test]
    fn file_change_approval_rejects_drift_before_commit() {
        let root = unique_dir("approval-drift-root");
        fs::write(root.join("note.txt"), "old\n").expect("write file");
        let tools = registry(&root);
        let call = call(
            "edit_file",
            json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
        );
        let request = approval_request(tools.execute(&call));
        fs::write(root.join("note.txt"), "old\nextra\n").expect("change after approval");

        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("approval no longer matches")
        );
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read drifted file"),
            "old\nextra\n"
        );
    }

    #[test]
    fn write_file_rejects_missing_parent_directory() {
        let root = unique_dir("write-missing-parent-root");
        let tools = registry(&root);

        let value = content(tools.execute(&call(
            "write_file",
            json!({"path": "missing/note.txt", "content": "new\n"}),
        )));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("failed to resolve parent directory")
        );
        assert!(!root.join("missing").exists());
    }

    #[test]
    fn read_only_rejects_file_write_tools() {
        let root = unique_dir("read-only-tools-root");
        fs::write(root.join("note.txt"), "old\n").expect("write file");
        let tools =
            registry_with_permissions(&root, PermissionProfile::for_mode(PermissionMode::ReadOnly));

        let edit = content(tools.execute(&call(
            "edit_file",
            json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
        )));
        let write = content(tools.execute(&call(
            "write_file",
            json!({"path": "created.txt", "content": "created\n"}),
        )));

        assert_eq!(edit["ok"], false);
        assert!(
            edit["error"]
                .as_str()
                .expect("error")
                .contains("file writes are denied")
        );
        assert_eq!(write["ok"], false);
        assert!(
            write["error"]
                .as_str()
                .expect("error")
                .contains("file writes are denied")
        );
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read file"),
            "old\n"
        );
        assert!(!root.join("created.txt").exists());
    }

    #[test]
    fn workspace_write_rejects_file_write_tools_outside_workspace() {
        let root = unique_dir("workspace-write-tools-root");
        let outside = outside_path(&root, "outside.txt");
        fs::write(&outside, "old\n").expect("write outside");
        let tools = registry(&root);

        let edit = content(tools.execute(&call(
            "edit_file",
            json!({"path": outside.display().to_string(), "old_text": "old", "new_text": "new"}),
        )));
        let write = content(tools.execute(&call(
            "write_file",
            json!({"path": outside.display().to_string(), "content": "new\n", "overwrite": true}),
        )));

        assert_eq!(edit["ok"], false);
        assert!(
            edit["error"]
                .as_str()
                .expect("error")
                .contains("outside the workspace root")
        );
        assert_eq!(write["ok"], false);
        assert!(
            write["error"]
                .as_str()
                .expect("error")
                .contains("outside the workspace root")
        );
        assert_eq!(fs::read_to_string(outside).expect("read outside"), "old\n");
    }

    #[test]
    fn danger_full_access_can_write_absolute_paths_outside_workspace() {
        let root = unique_dir("danger-write-root");
        let outside = outside_path(&root, "outside-danger.txt");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let value = content(tools.execute(&call(
            "write_file",
            json!({"path": outside.display().to_string(), "content": "outside\n"}),
        )));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["created"], true);
        assert_eq!(value["data"]["overwritten"], false);
        assert_eq!(
            fs::read_to_string(outside).expect("read outside"),
            "outside\n"
        );
    }

    #[test]
    fn apply_patch_adds_updates_and_deletes_files() {
        let root = unique_dir("patch-basic-root");
        fs::write(root.join("update.txt"), "alpha\nbeta\ngamma\n").expect("write update");
        fs::write(root.join("delete.txt"), "delete me\n").expect("write delete");
        let tools = registry(&root);

        let call = patch_call(
            r#"*** Begin Patch
*** Add File: added.txt
+hello
+world
*** Update File: update.txt
@@
 alpha
-beta
+BETA
 gamma
*** Delete File: delete.txt
*** End Patch"#,
        );
        let request = approval_request(tools.execute(&call));
        assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
        assert!(!root.join("added.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("update.txt")).expect("read before approval"),
            "alpha\nbeta\ngamma\n"
        );
        assert!(root.join("delete.txt").exists());
        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["changed_files"], 3);
        assert_eq!(
            fs::read_to_string(root.join("added.txt")).expect("read added"),
            "hello\nworld\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("update.txt")).expect("read update"),
            "alpha\nBETA\ngamma\n"
        );
        assert!(!root.join("delete.txt").exists());
        let files = value["data"]["files"].as_array().expect("files");
        assert_eq!(files[0]["operation"], "add");
        assert_eq!(files[1]["operation"], "update");
        assert_eq!(files[1]["replacements"], 1);
        assert_eq!(files[2]["operation"], "delete");
    }

    #[test]
    fn apply_patch_updates_multiple_files_and_hunks() {
        let root = unique_dir("patch-multi-root");
        fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("write a");
        fs::write(root.join("b.txt"), "red\nblue\n").expect("write b");
        let tools = registry(&root);

        let call = patch_call(
            r#"*** Begin Patch
*** Update File: a.txt
@@
 one
-two
+TWO
 three
@@
 three
-four
+FOUR
*** Update File: b.txt
@@
-red
+RED
 blue
*** End Patch"#,
        );
        let value = approved_content(&tools, &call);

        assert_eq!(value["ok"], true);
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).expect("read a"),
            "one\nTWO\nthree\nFOUR\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("b.txt")).expect("read b"),
            "RED\nblue\n"
        );
        let files = value["data"]["files"].as_array().expect("files");
        assert_eq!(files[0]["replacements"], 2);
        assert_eq!(files[1]["replacements"], 1);
    }

    #[test]
    fn apply_patch_rejects_invalid_targets() {
        let root = unique_dir("patch-invalid-targets-root");
        fs::write(root.join("existing.txt"), "old\n").expect("write existing");
        fs::create_dir(root.join("dir")).expect("create dir");
        fs::write(root.join("binary.bin"), [0xff, 0xfe]).expect("write binary");
        let tools = registry(&root);

        let add_existing = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Add File: existing.txt
+new
*** End Patch"#,
        )));
        assert_eq!(add_existing["ok"], false);
        assert!(
            add_existing["error"]
                .as_str()
                .expect("error")
                .contains("already exists")
        );

        let update_missing = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: missing.txt
@@
-old
+new
*** End Patch"#,
        )));
        assert_eq!(update_missing["ok"], false);
        assert!(
            update_missing["error"]
                .as_str()
                .expect("error")
                .contains("failed to inspect")
        );

        let delete_missing = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Delete File: missing.txt
*** End Patch"#,
        )));
        assert_eq!(delete_missing["ok"], false);
        assert!(
            delete_missing["error"]
                .as_str()
                .expect("error")
                .contains("failed to inspect")
        );

        let update_dir = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: dir
@@
-old
+new
*** End Patch"#,
        )));
        assert_eq!(update_dir["ok"], false);
        assert!(
            update_dir["error"]
                .as_str()
                .expect("error")
                .contains("is not a file")
        );

        let update_binary = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: binary.bin
@@
-old
+new
*** End Patch"#,
        )));
        assert_eq!(update_binary["ok"], false);
        assert!(
            update_binary["error"]
                .as_str()
                .expect("error")
                .contains("UTF-8")
        );
    }

    #[test]
    fn apply_patch_rejects_invalid_update_hunks() {
        let root = unique_dir("patch-invalid-hunks-root");
        fs::write(root.join("no-match.txt"), "alpha\n").expect("write no match");
        fs::write(root.join("many.txt"), "alpha\nalpha\n").expect("write many");
        fs::write(root.join("same.txt"), "alpha\n").expect("write same");
        let tools = registry(&root);

        let no_match = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: no-match.txt
@@
-beta
+gamma
*** End Patch"#,
        )));
        assert_eq!(no_match["ok"], false);
        assert!(
            no_match["error"]
                .as_str()
                .expect("error")
                .contains("found 0")
        );

        let many = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: many.txt
@@
-alpha
+beta
*** End Patch"#,
        )));
        assert_eq!(many["ok"], false);
        assert!(many["error"].as_str().expect("error").contains("found 2"));

        let empty = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: same.txt
@@
*** End Patch"#,
        )));
        assert_eq!(empty["ok"], false);
        assert!(
            empty["error"]
                .as_str()
                .expect("error")
                .contains("empty hunk")
        );

        let no_old_text = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: same.txt
@@
+insert
*** End Patch"#,
        )));
        assert_eq!(no_old_text["ok"], false);
        assert!(
            no_old_text["error"]
                .as_str()
                .expect("error")
                .contains("must include context or removed lines")
        );

        let no_change = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: same.txt
@@
 alpha
*** End Patch"#,
        )));
        assert_eq!(no_change["ok"], false);
        assert!(
            no_change["error"]
                .as_str()
                .expect("error")
                .contains("has no changes")
        );
    }

    #[test]
    fn apply_patch_rejects_invalid_patch_syntax() {
        let root = unique_dir("patch-invalid-syntax-root");
        let tools = registry(&root);

        for patch in [
            "*** Add File: a.txt\n+x\n*** End Patch",
            "*** Begin Patch\n*** Add File: a.txt\n+x",
            "*** Begin Patch\n*** Move to: b.txt\n*** End Patch",
            "*** Begin Patch\n*** Rename File: a.txt\n*** End Patch",
            "*** Begin Patch\n*** Add File: a.txt\nx\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n?bad\n*** End Patch",
        ] {
            let value = content(tools.execute(&patch_call(patch)));
            assert_eq!(value["ok"], false, "patch should fail: {patch}");
        }
    }

    #[test]
    fn apply_patch_rejects_duplicate_paths_and_preserves_files() {
        let root = unique_dir("patch-duplicate-root");
        fs::write(root.join("same.txt"), "old\n").expect("write same");
        let tools = registry(&root);

        let value = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: same.txt
@@
-old
+new
*** Delete File: ./same.txt
*** End Patch"#,
        )));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("more than once")
        );
        assert_eq!(
            fs::read_to_string(root.join("same.txt")).expect("read same"),
            "old\n"
        );
    }

    #[test]
    fn apply_patch_validation_failure_preserves_all_files() {
        let root = unique_dir("patch-atomic-validation-root");
        fs::write(root.join("first.txt"), "old\n").expect("write first");
        fs::write(root.join("second.txt"), "keep\n").expect("write second");
        let tools = registry(&root);

        let value = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Update File: first.txt
@@
-old
+new
*** Update File: second.txt
@@
-missing
+changed
*** End Patch"#,
        )));

        assert_eq!(value["ok"], false);
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).expect("read first"),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("second.txt")).expect("read second"),
            "keep\n"
        );
    }

    #[test]
    fn read_only_rejects_apply_patch() {
        let root = unique_dir("patch-read-only-root");
        let tools =
            registry_with_permissions(&root, PermissionProfile::for_mode(PermissionMode::ReadOnly));

        let value = content(tools.execute(&patch_call(
            r#"*** Begin Patch
*** Add File: created.txt
+content
*** End Patch"#,
        )));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("file writes are denied")
        );
        assert!(!root.join("created.txt").exists());
    }

    #[test]
    fn workspace_write_rejects_apply_patch_outside_workspace() {
        let root = unique_dir("patch-workspace-write-root");
        let outside = outside_path(&root, "outside-patch.txt");
        let tools = registry(&root);

        let value = content(tools.execute(&patch_call(&format!(
            "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
            outside.display()
        ))));

        assert_eq!(value["ok"], false);
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("outside the workspace root")
        );
        assert!(!outside.exists());
    }

    #[test]
    fn danger_full_access_can_apply_patch_outside_workspace() {
        let root = unique_dir("patch-danger-root");
        let outside = outside_path(&root, "outside-patch-danger.txt");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let value = content(tools.execute(&patch_call(&format!(
            "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
            outside.display()
        ))));

        assert_eq!(value["ok"], true);
        assert_eq!(
            fs::read_to_string(outside).expect("read outside"),
            "outside\n"
        );
    }

    #[test]
    fn shell_command_runs_in_workspace_and_reports_exit_code() {
        let root = unique_dir("shell-root");
        let tools = registry_with_permissions(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        );

        let result = completed_result(tools.execute(&call(
            "shell_command",
            json!({"command": "pwd && exit 7", "timeout_secs": 5}),
        )));
        let value: Value = serde_json::from_str(&result.content).expect("tool JSON");
        let shell = result
            .summary
            .as_ref()
            .and_then(|summary| summary.shell.as_ref())
            .expect("shell summary");

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["exit_code"], 7);
        assert_eq!(value["data"]["timed_out"], false);
        assert_eq!(shell.exit_code, Some(7));
        assert!(!shell.timed_out);
        assert!(!shell.stdout_truncated);
        assert!(!shell.stderr_truncated);
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

        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::approve(request.id.clone()),
            &request,
        ));

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

        let value = content(tools.execute_approved(
            &call,
            &ApprovalDecision::deny(request.id.clone()),
            &request,
        ));

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
