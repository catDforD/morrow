use super::*;

const MAX_FILE_DIFF_LINES: usize = 240;
const MAX_FILE_DIFF_BYTES: usize = 20_000;
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
pub(crate) fn file_change_summary_json(summary: &FileChangeSummary) -> Value {
    json!({
        "path": summary.path,
        "operation": summary.operation.as_str(),
        "replacements": summary.replacements,
        "created": summary.created,
        "overwritten": summary.overwritten,
        "deleted": summary.deleted,
    })
}

pub(crate) fn render_file_diff(changes: &[StagedPatchChange], tools: &BuiltInTools) -> String {
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
    tools: &BuiltInTools,
) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .map_err(|err| {
            format!(
                "failed to create temporary file for {}: {err}",
                tools.display_path(display_path)
            )
        })?;
    if let Err(err) = file.write_all(content.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(temp_path);
        return Err(format!(
            "failed to write temporary file for {}: {err}",
            tools.display_path(display_path)
        ));
    }
    drop(file);
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

pub(crate) fn commit_patch_changes(
    mut changes: Vec<StagedPatchChange>,
    tools: &BuiltInTools,
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
                let Some(temp_path) = change.temp_path.take() else {
                    return fail_patch_commit(
                        "staged add file is missing temporary content".to_string(),
                        &changes,
                        applied,
                        tools,
                    );
                };
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
                let Some(temp_path) = change.temp_path.take() else {
                    return fail_patch_commit(
                        "staged update file is missing temporary content".to_string(),
                        &changes,
                        applied,
                        tools,
                    );
                };
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
                applied.push(AppliedPatchChange {
                    path: change.path.clone(),
                    kind: PatchOperationKind::Update,
                    backup_path: Some(backup_path.clone()),
                });
                if let Err(err) = fs::rename(&temp_path, &change.path) {
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
    tools: &BuiltInTools,
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
    tools: &BuiltInTools,
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
                match fs::remove_file(&change.path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        errors.push(format!(
                            "failed to remove updated {}: {err}",
                            tools.display_path(&change.path)
                        ));
                    }
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

pub(crate) fn parse_patch(patch: &str) -> Result<Vec<ParsedPatchOperation>, String> {
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
pub(crate) enum ParsedPatchOperation {
    Add { path: String, content: String },
    Update { path: String, hunks: Vec<PatchHunk> },
    Delete { path: String },
}

impl ParsedPatchOperation {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } | Self::Delete { path } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchHunk {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchOperationKind {
    Add,
    Update,
    Delete,
}

#[derive(Debug)]
pub(crate) struct StagedPatchChange {
    pub(crate) path: PathBuf,
    pub(crate) kind: PatchOperationKind,
    pub(crate) content: Option<String>,
    pub(crate) permissions: Option<fs::Permissions>,
    pub(crate) summary: FileChangeSummary,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) temp_path: Option<PathBuf>,
}

impl StagedPatchChange {
    pub(crate) fn write(
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

    pub(crate) fn delete(
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
pub(crate) struct FileChangePlan {
    pub(crate) changes: Vec<StagedPatchChange>,
    pub(crate) data: Value,
    pub(crate) files: Vec<FileChangeSummary>,
    pub(crate) diff: String,
    pub(crate) summary: ToolExecutionSummary,
}

#[derive(Debug)]
struct AppliedPatchChange {
    path: PathBuf,
    kind: PatchOperationKind,
    backup_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApplyPatchArgs {
    pub(crate) patch: String,
}
