use super::*;

pub(crate) fn handle_session_command(
    command: &SessionCommand,
    default_session_name: &str,
    workspace_root: &Path,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    match command {
        SessionCommand::List => {
            let store = SessionStore::for_workspace(workspace_root, default_session_name)?;
            let entries = store.list_current_scope()?;
            if entries.is_empty() {
                writeln!(stdout, "no sessions").map_err(CliError::Stdout)?;
            } else {
                writeln!(stdout, "NAME\tTURNS\tACTIVE_MESSAGES\tSUMMARY\tPATH")
                    .map_err(CliError::Stdout)?;
                for entry in entries {
                    writeln!(
                        stdout,
                        "{}\t{}\t{}\t{}\t{}",
                        entry.name,
                        entry.turns,
                        entry.active_messages,
                        if entry.has_summary { "yes" } else { "no" },
                        entry.path.display()
                    )
                    .map_err(CliError::Stdout)?;
                }
            }
        }
        SessionCommand::Show { name } => {
            let name = name.as_deref().unwrap_or(default_session_name);
            let store = SessionStore::for_workspace(workspace_root, name)?;
            let session = store.load_existing()?;
            writeln!(stdout, "name: {name}").map_err(CliError::Stdout)?;
            writeln!(stdout, "path: {}", store.path().display()).map_err(CliError::Stdout)?;
            writeln!(stdout, "turns: {}", session.turns.len()).map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "active_messages: {}",
                session.active_thread.messages.len()
            )
            .map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "summarized_turns: {}",
                session.context.summarized_turns
            )
            .map_err(CliError::Stdout)?;
            writeln!(
                stdout,
                "summary: {}",
                if session.context.summary.is_some() {
                    "yes"
                } else {
                    "no"
                }
            )
            .map_err(CliError::Stdout)?;
        }
        SessionCommand::Delete { name } => {
            let store = SessionStore::for_workspace(workspace_root, name)?;
            store.delete()?;
            SubagentSessionStore::for_workspace(workspace_root, name)?.delete_all()?;
            writeln!(stdout, "deleted session: {name}").map_err(CliError::Stdout)?;
        }
        SessionCommand::Rename { old, new } => {
            let store = SessionStore::for_workspace(workspace_root, old)?;
            let subagents = SubagentSessionStore::for_workspace(workspace_root, old)?;
            let renamed_subagents = subagents.rename(new)?;
            let target = match store.rename(new) {
                Ok(target) => target,
                Err(error) => {
                    let _ = renamed_subagents.rename(old);
                    return Err(error.into());
                }
            };
            writeln!(
                stdout,
                "renamed session: {old} -> {new} ({})",
                target.path().display()
            )
            .map_err(CliError::Stdout)?;
        }
        SessionCommand::Export { name, output } => {
            let name = name.as_deref().unwrap_or(default_session_name);
            let store = SessionStore::for_workspace(workspace_root, name)?;
            let bytes = store.export_document_bytes()?;
            if let Some(path) = output {
                if path.exists() {
                    return Err(CliError::OutputExists { path: path.clone() });
                }
                fs::write(path, &bytes).map_err(|source| CliError::OutputWrite {
                    path: path.clone(),
                    source,
                })?;
                eprintln!("exported session: {name} -> {}", path.display());
            } else {
                stdout.write_all(&bytes).map_err(CliError::Stdout)?;
                stdout.write_all(b"\n").map_err(CliError::Stdout)?;
            }
        }
    }

    stdout.flush().map_err(CliError::Stdout)
}
