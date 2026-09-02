use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode<'a> {
    Human,
    Jsonl {
        session_name: &'a str,
        turn_index: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionRecord {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) summary: Option<ToolExecutionSummary>,
}

pub(crate) fn write_jsonl_event(
    stdout: &mut dyn Write,
    envelope: &AgentEventEnvelope,
) -> Result<(), CliError> {
    serde_json::to_writer(&mut *stdout, envelope).map_err(CliError::JsonlSerialize)?;
    stdout.write_all(b"\n").map_err(CliError::Stdout)?;
    stdout.flush().map_err(CliError::Stdout)?;
    Ok(())
}

pub(crate) fn warn_if_lossy_input(input: &InputLine) {
    if input.had_invalid_utf8 {
        eprintln!("warning: stdin contained invalid UTF-8; replaced invalid bytes");
    }
}

pub(crate) fn print_execution_summary(records: &[ExecutionRecord]) {
    if let Some(summary) = format_execution_summary(records) {
        eprint!("{summary}");
    }
}

pub(crate) fn format_execution_summary(records: &[ExecutionRecord]) -> Option<String> {
    if records.is_empty() {
        return None;
    }

    let mut output = String::from("execution summary:\n");
    for record in records {
        let status = if record.ok { "ok" } else { "error" };
        let _ = writeln!(output, "- {}: {status}", record.name);
        if let Some(summary) = record.summary.as_ref() {
            if !summary.files.is_empty() {
                append_file_list(&mut output, &summary.files);
                if summary.diff.as_deref().is_some_and(|diff| !diff.is_empty()) {
                    let _ = writeln!(output, "  diff: available");
                }
            }
            if let Some(shell) = summary.shell.as_ref() {
                append_shell_summary(&mut output, shell);
            }
            if let Some(error) = summary.error.as_ref() {
                let _ = writeln!(output, "  error: {error}");
            }
            if let Some(subagent) = summary.subagent.as_ref() {
                let _ = writeln!(
                    output,
                    "  agent: {}",
                    subagent_name(subagent.agent_name.as_deref())
                );
                let _ = writeln!(output, "  task: {}", compact_line(&subagent.task, 160));
                let _ = writeln!(
                    output,
                    "  subagent: model_calls={}, tool_calls={}, truncated={}",
                    subagent.model_calls, subagent.tool_calls, subagent.truncated
                );
                if let Some(error) = subagent.error.as_ref() {
                    let _ = writeln!(output, "  error: {error}");
                }
            }
        }
    }

    Some(output)
}

pub(crate) fn compact_line(value: &str, max_chars: usize) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let mut compact = one_line
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    compact.push('…');
    compact
}

pub(crate) fn append_file_list(output: &mut String, files: &[FileChangeSummary]) {
    if files.is_empty() {
        let _ = writeln!(output, "files: none");
        return;
    }

    let _ = writeln!(output, "files:");
    for file in files {
        let _ = writeln!(
            output,
            "- {} ({}, replacements={}, created={}, overwritten={}, deleted={})",
            file.path,
            file.operation.as_str(),
            file.replacements,
            file.created,
            file.overwritten,
            file.deleted
        );
    }
}

fn append_shell_summary(output: &mut String, shell: &ShellCommandSummary) {
    let exit_code = shell
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_string());
    let _ = writeln!(
        output,
        "  shell: exit_code={exit_code}, timed_out={}, stdout_truncated={}, stderr_truncated={}",
        shell.timed_out, shell.stdout_truncated, shell.stderr_truncated
    );
}
