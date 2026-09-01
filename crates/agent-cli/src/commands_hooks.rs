use super::*;

pub(crate) fn handle_hooks_command(
    command: &HooksCommand,
    workspace_root: &Path,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let manager = HookManager::for_workspace(workspace_root)?;
    handle_hooks_command_with_manager(command, &manager, stdout)
}

pub(crate) fn handle_hooks_command_with_manager(
    command: &HooksCommand,
    manager: &HookManager,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let settings = match command {
        HooksCommand::List { json } => {
            let settings = manager.settings()?;
            if *json {
                serde_json::to_writer_pretty(&mut *stdout, &settings)
                    .map_err(CliError::JsonlSerialize)?;
                stdout.write_all(b"\n").map_err(CliError::Stdout)?;
                return stdout.flush().map_err(CliError::Stdout);
            }
            settings
        }
        HooksCommand::Trust => manager.trust_project()?,
        HooksCommand::Revoke => manager.revoke_project()?,
    };
    write_hook_settings(stdout, &settings)?;
    stdout.flush().map_err(CliError::Stdout)
}

fn write_hook_settings(stdout: &mut dyn Write, settings: &HookSettings) -> Result<(), CliError> {
    let project_status = match (
        settings.project_fingerprint.as_ref(),
        settings.project_trusted,
    ) {
        (None, _) => "not configured",
        (Some(_), true) => "trusted",
        (Some(_), false) => "not trusted",
    };
    writeln!(
        stdout,
        "project hooks: {}{}",
        project_status,
        settings
            .project_fingerprint
            .as_ref()
            .map(|fingerprint| format!(" ({fingerprint})"))
            .unwrap_or_default()
    )
    .map_err(CliError::Stdout)?;
    for hook in &settings.hooks {
        writeln!(
            stdout,
            "{}\t{}\t{:?}\t{}\t{}",
            hook.id,
            hook.event.as_str(),
            hook.source,
            if hook.active { "active" } else { "disabled" },
            hook.command.join(" ")
        )
        .map_err(CliError::Stdout)?;
    }
    for diagnostic in &settings.diagnostics {
        writeln!(stdout, "warning: {diagnostic}").map_err(CliError::Stdout)?;
    }
    Ok(())
}
