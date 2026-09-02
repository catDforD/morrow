use super::*;

pub(crate) fn approval_decision(
    request: &ApprovalRequest,
    permissions: PermissionProfile,
    interactive: bool,
) -> Result<ApprovalDecision, CliError> {
    print_approval_request(request, permissions);

    if !interactive {
        eprintln!("stdin is not interactive; approval denied by default");
        return Ok(ApprovalDecision::deny(request.id.clone()));
    }

    eprint!("approve this action? [y/N] ");
    io::stderr().flush().map_err(CliError::Stderr)?;

    let input = read_stdin_line()?.unwrap_or_else(|| InputLine {
        text: String::new(),
        had_invalid_utf8: false,
    });
    warn_if_lossy_input(&input);
    let approved = matches!(input.text.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    Ok(if approved {
        ApprovalDecision::approve(request.id.clone())
    } else {
        ApprovalDecision::deny(request.id.clone())
    })
}

fn print_approval_request(request: &ApprovalRequest, permissions: PermissionProfile) {
    eprint!("{}", format_approval_request(request, permissions));
}

pub(crate) fn format_approval_request(
    request: &ApprovalRequest,
    permissions: PermissionProfile,
) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "approval required: {}", request.reason);
    match &request.action {
        ApprovalAction::ShellCommand {
            command,
            cwd,
            timeout_secs,
        } => {
            let _ = writeln!(output, "action: shell command");
            let _ = writeln!(output, "command: {command}");
            let _ = writeln!(output, "cwd: {}", cwd.display());
            let _ = writeln!(output, "timeout: {timeout_secs}s");
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(
                output,
                "warning: approving this command may modify files or access the network."
            );
        }
        ApprovalAction::FileChanges { files, diff } => {
            let _ = writeln!(output, "action: file changes");
            append_file_list(&mut output, files);
            let _ = writeln!(output, "diff:");
            output.push_str(diff);
            if !diff.ends_with('\n') {
                output.push('\n');
            }
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(output, "warning: approving this action will modify files.");
        }
        ApprovalAction::McpTool {
            server,
            tool,
            arguments,
        } => {
            let _ = writeln!(output, "action: mcp tool");
            let _ = writeln!(output, "server: {server}");
            let _ = writeln!(output, "tool: {tool}");
            let _ = writeln!(output, "arguments: {arguments}");
            let _ = writeln!(output, "permissions: {}", permission_summary(permissions));
            let _ = writeln!(
                output,
                "warning: approving this action lets the MCP server perform side effects."
            );
        }
    }
    output
}
