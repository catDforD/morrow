use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::Mutex;

const MAX_COMMAND_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_ARGUMENT_HINT_CHARS: usize = 120;
const MAX_PROMPT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandSettingsResponse {
    pub commands: Vec<CommandResponse>,
    pub store_path: String,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandResponse {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandWriteRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub argument_hint: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveCommandRequest {
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolveCommandResponse {
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_name: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Error)]
pub enum CommandRegistryError {
    #[error("failed to create command directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to list command directory {path}: {source}")]
    List {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write command file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace command file {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove command file {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid command: {0}")]
    Validation(String),
    #[error("command {0:?} already exists")]
    Conflict(String),
    #[error("command {0:?} was not found")]
    NotFound(String),
}

pub struct CommandRegistry {
    root: PathBuf,
    mutation: Mutex<()>,
}

impl CommandRegistry {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            mutation: Mutex::new(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn settings(&self) -> Result<CommandSettingsResponse, CommandRegistryError> {
        let (commands, diagnostics) = self.load_commands()?;
        Ok(CommandSettingsResponse {
            commands,
            store_path: self.root.display().to_string(),
            diagnostics,
        })
    }

    pub async fn create(
        &self,
        request: CommandWriteRequest,
    ) -> Result<CommandResponse, CommandRegistryError> {
        let _guard = self.mutation.lock().await;
        let command = normalize_request(request)?;
        ensure_root(&self.root)?;
        let path = command_path(&self.root, &command.name)?;
        if path.exists() {
            return Err(CommandRegistryError::Conflict(command.name));
        }
        write_command(&path, &command)?;
        Ok(command)
    }

    pub async fn update(
        &self,
        current_name: &str,
        request: CommandWriteRequest,
    ) -> Result<CommandResponse, CommandRegistryError> {
        let _guard = self.mutation.lock().await;
        let current_path = command_path(&self.root, current_name)?;
        if !current_path.is_file() {
            return Err(CommandRegistryError::NotFound(current_name.to_string()));
        }
        let command = normalize_request(request)?;
        let target_path = command_path(&self.root, &command.name)?;
        if target_path != current_path && target_path.exists() {
            return Err(CommandRegistryError::Conflict(command.name));
        }
        write_command(&target_path, &command)?;
        if target_path != current_path
            && let Err(source) = fs::remove_file(&current_path)
        {
            let _ = fs::remove_file(&target_path);
            return Err(CommandRegistryError::Remove {
                path: current_path,
                source,
            });
        }
        Ok(command)
    }

    pub async fn delete(&self, name: &str) -> Result<(), CommandRegistryError> {
        let _guard = self.mutation.lock().await;
        let path = command_path(&self.root, name)?;
        if !path.is_file() {
            return Err(CommandRegistryError::NotFound(name.to_string()));
        }
        fs::remove_file(&path).map_err(|source| CommandRegistryError::Remove { path, source })
    }

    pub fn resolve(
        &self,
        request: ResolveCommandRequest,
    ) -> Result<ResolveCommandResponse, CommandRegistryError> {
        let input = request.input.trim().to_string();
        if let Some(literal) = input.strip_prefix("//") {
            return Ok(ResolveCommandResponse {
                matched: false,
                command_name: None,
                prompt: format!("/{literal}"),
            });
        }
        let Some(invocation) = input.strip_prefix('/') else {
            return Ok(unmatched(input));
        };
        let command_end = invocation
            .find(char::is_whitespace)
            .unwrap_or(invocation.len());
        let name = &invocation[..command_end];
        if name.is_empty() || validate_command_name(name).is_err() {
            return Ok(unmatched(input));
        }
        let (commands, _) = self.load_commands()?;
        let Some(command) = commands.into_iter().find(|command| command.name == name) else {
            return Ok(unmatched(input));
        };
        let arguments = invocation[command_end..].trim();
        let has_placeholder = command.prompt.contains("$ARGUMENTS");
        let mut prompt = command.prompt.replace("$ARGUMENTS", arguments);
        if !has_placeholder && !arguments.is_empty() {
            prompt.push_str("\n\nArguments:\n");
            prompt.push_str(arguments);
        }
        Ok(ResolveCommandResponse {
            matched: true,
            command_name: Some(command.name),
            prompt,
        })
    }

    fn load_commands(&self) -> Result<(Vec<CommandResponse>, Vec<String>), CommandRegistryError> {
        if !self.root.is_dir() {
            return Ok((Vec::new(), Vec::new()));
        }
        let entries = fs::read_dir(&self.root).map_err(|source| CommandRegistryError::List {
            path: self.root.clone(),
            source,
        })?;
        let mut commands = Vec::new();
        let mut diagnostics = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(format!("failed to read command directory entry: {error}"));
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                diagnostics.push(format!(
                    "ignored command with invalid filename: {}",
                    path.display()
                ));
                continue;
            };
            if let Err(error) = validate_command_name(name) {
                diagnostics.push(format!("ignored {}: {error}", path.display()));
                continue;
            }
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    diagnostics.push(format!("failed to inspect {}: {error}", path.display()));
                    continue;
                }
            };
            if metadata.len() > MAX_PROMPT_BYTES as u64 + 4096 {
                diagnostics.push(format!(
                    "ignored {}: command file is too large",
                    path.display()
                ));
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    diagnostics.push(format!("failed to read {}: {error}", path.display()));
                    continue;
                }
            };
            match parse_command(name, &content) {
                Ok(command) => commands.push(command),
                Err(error) => diagnostics.push(format!("ignored {}: {error}", path.display())),
            }
        }
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        Ok((commands, diagnostics))
    }
}

fn unmatched(prompt: String) -> ResolveCommandResponse {
    ResolveCommandResponse {
        matched: false,
        command_name: None,
        prompt,
    }
}

fn ensure_root(root: &Path) -> Result<(), CommandRegistryError> {
    fs::create_dir_all(root).map_err(|source| CommandRegistryError::CreateDir {
        path: root.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(|source| {
            CommandRegistryError::CreateDir {
                path: root.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn command_path(root: &Path, name: &str) -> Result<PathBuf, CommandRegistryError> {
    validate_command_name(name)?;
    Ok(root.join(format!("{name}.md")))
}

fn validate_command_name(name: &str) -> Result<(), CommandRegistryError> {
    let bytes = name.as_bytes();
    let valid_first = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if bytes.is_empty()
        || bytes.len() > MAX_COMMAND_NAME_BYTES
        || !valid_first
        || bytes.iter().any(|byte| {
            !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && !matches!(byte, b'-' | b'_')
        })
    {
        return Err(CommandRegistryError::Validation(
            "name must be 1-64 lowercase ASCII letters, digits, '-' or '_'".to_string(),
        ));
    }
    Ok(())
}

fn normalize_request(
    request: CommandWriteRequest,
) -> Result<CommandResponse, CommandRegistryError> {
    let name = request.name.trim().to_string();
    validate_command_name(&name)?;
    let description = request.description.trim().to_string();
    let argument_hint = request.argument_hint.trim().to_string();
    let prompt = request.prompt.trim().to_string();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(CommandRegistryError::Validation(format!(
            "description must not exceed {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    if argument_hint.chars().count() > MAX_ARGUMENT_HINT_CHARS {
        return Err(CommandRegistryError::Validation(format!(
            "argument hint must not exceed {MAX_ARGUMENT_HINT_CHARS} characters"
        )));
    }
    if prompt.is_empty() {
        return Err(CommandRegistryError::Validation(
            "prompt must not be empty".to_string(),
        ));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(CommandRegistryError::Validation(format!(
            "prompt must not exceed {MAX_PROMPT_BYTES} bytes"
        )));
    }
    Ok(CommandResponse {
        name,
        description,
        argument_hint,
        prompt,
    })
}

fn write_command(path: &Path, command: &CommandResponse) -> Result<(), CommandRegistryError> {
    let parent = path.parent().ok_or_else(|| {
        CommandRegistryError::Validation("command path has no parent".to_string())
    })?;
    ensure_root(parent)?;
    let content = render_command(command)?;
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("command.md"),
        std::process::id()
    ));
    fs::write(&temp, content).map_err(|source| CommandRegistryError::Write {
        path: temp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|source| {
            CommandRegistryError::Write {
                path: temp.clone(),
                source,
            }
        })?;
    }
    fs::rename(&temp, path).map_err(|source| CommandRegistryError::Replace {
        path: path.to_path_buf(),
        source,
    })
}

fn render_command(command: &CommandResponse) -> Result<String, CommandRegistryError> {
    if command.description.is_empty() && command.argument_hint.is_empty() {
        return Ok(format!("{}\n", command.prompt));
    }
    let description = serde_json::to_string(&command.description).map_err(|error| {
        CommandRegistryError::Validation(format!("failed to encode description: {error}"))
    })?;
    let argument_hint = serde_json::to_string(&command.argument_hint).map_err(|error| {
        CommandRegistryError::Validation(format!("failed to encode argument hint: {error}"))
    })?;
    Ok(format!(
        "---\ndescription: {description}\nargument-hint: {argument_hint}\n---\n{}\n",
        command.prompt
    ))
}

fn parse_command(name: &str, content: &str) -> Result<CommandResponse, CommandRegistryError> {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut description = String::new();
    let mut argument_hint = String::new();
    let prompt = if normalized.lines().next() == Some("---") {
        let mut offset = normalized
            .find('\n')
            .map(|index| index + 1)
            .ok_or_else(|| {
                CommandRegistryError::Validation("frontmatter is not closed".to_string())
            })?;
        let mut closed = false;
        while offset <= normalized.len() {
            let remaining = &normalized[offset..];
            let line_end = remaining.find('\n').unwrap_or(remaining.len());
            let line = remaining[..line_end].trim_end_matches('\r');
            offset += line_end + usize::from(line_end < remaining.len());
            if line == "---" {
                closed = true;
                break;
            }
            let Some((key, raw_value)) = line.split_once(':') else {
                return Err(CommandRegistryError::Validation(format!(
                    "invalid frontmatter line {line:?}"
                )));
            };
            let value = parse_frontmatter_value(raw_value.trim())?;
            match key.trim() {
                "description" => description = value,
                "argument-hint" => argument_hint = value,
                _ => {}
            }
        }
        if !closed {
            return Err(CommandRegistryError::Validation(
                "frontmatter is not closed".to_string(),
            ));
        }
        normalized[offset..].trim()
    } else {
        normalized.trim()
    };
    normalize_request(CommandWriteRequest {
        name: name.to_string(),
        description,
        argument_hint,
        prompt: prompt.to_string(),
    })
}

fn parse_frontmatter_value(value: &str) -> Result<String, CommandRegistryError> {
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value).map_err(|error| {
            CommandRegistryError::Validation(format!("invalid quoted frontmatter value: {error}"))
        });
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("morrow-commands-{name}-{stamp}"))
    }

    fn request(name: &str, prompt: &str) -> CommandWriteRequest {
        CommandWriteRequest {
            name: name.to_string(),
            description: "description".to_string(),
            argument_hint: "<path>".to_string(),
            prompt: prompt.to_string(),
        }
    }

    #[tokio::test]
    async fn command_files_round_trip_with_frontmatter() {
        let root = unique_root("round-trip");
        let registry = CommandRegistry::new(root.clone());
        registry
            .create(request("review", "Review $ARGUMENTS"))
            .await
            .expect("create");

        let settings = registry.settings().expect("settings");

        assert_eq!(settings.commands[0].name, "review");
        assert_eq!(settings.commands[0].argument_hint, "<path>");
        assert!(root.join("review.md").is_file());
    }

    #[test]
    fn plain_markdown_without_frontmatter_is_supported() {
        let command = parse_command("review", "Review this repository\n").expect("parse");

        assert_eq!(command.prompt, "Review this repository");
        assert!(command.description.is_empty());
    }

    #[tokio::test]
    async fn resolver_replaces_arguments_and_appends_when_missing_placeholder() {
        let root = unique_root("resolve");
        let registry = CommandRegistry::new(root);
        registry
            .create(request("review", "Review $ARGUMENTS twice: $ARGUMENTS"))
            .await
            .expect("create");
        registry
            .create(request("explain", "Explain the code"))
            .await
            .expect("create");

        let replaced = registry
            .resolve(ResolveCommandRequest {
                input: "/review src/lib.rs".to_string(),
            })
            .expect("resolve");
        let appended = registry
            .resolve(ResolveCommandRequest {
                input: "/explain src/lib.rs".to_string(),
            })
            .expect("resolve");

        assert_eq!(replaced.prompt, "Review src/lib.rs twice: src/lib.rs");
        assert_eq!(
            appended.prompt,
            "Explain the code\n\nArguments:\nsrc/lib.rs"
        );
    }

    #[tokio::test]
    async fn unknown_commands_are_unchanged_and_double_slash_escapes() {
        let registry = CommandRegistry::new(unique_root("unknown"));

        let unknown = registry
            .resolve(ResolveCommandRequest {
                input: "/missing value".to_string(),
            })
            .expect("resolve unknown");
        let escaped = registry
            .resolve(ResolveCommandRequest {
                input: "//review value".to_string(),
            })
            .expect("resolve escaped");

        assert!(!unknown.matched);
        assert_eq!(unknown.prompt, "/missing value");
        assert_eq!(escaped.prompt, "/review value");
    }

    #[tokio::test]
    async fn commands_can_be_renamed_and_deleted_without_overwriting() {
        let root = unique_root("rename-delete");
        let registry = CommandRegistry::new(root.clone());
        registry
            .create(request("review", "Review"))
            .await
            .expect("create review");
        registry
            .create(request("explain", "Explain"))
            .await
            .expect("create explain");

        let conflict = registry
            .update("review", request("explain", "Updated"))
            .await
            .expect_err("must not overwrite");
        assert!(matches!(conflict, CommandRegistryError::Conflict(_)));

        registry
            .update("review", request("audit", "Audit"))
            .await
            .expect("rename");
        assert!(!root.join("review.md").exists());
        assert!(root.join("audit.md").is_file());

        registry.delete("audit").await.expect("delete");
        assert!(!root.join("audit.md").exists());
    }

    #[test]
    fn malformed_files_are_skipped_with_diagnostics() {
        let root = unique_root("diagnostics");
        ensure_root(&root).expect("root");
        fs::write(root.join("broken.md"), "---\ndescription: \"broken\n")
            .expect("write broken command");
        fs::write(root.join("plain.md"), "Valid prompt\n").expect("write valid command");
        let registry = CommandRegistry::new(root);

        let settings = registry.settings().expect("settings");

        assert_eq!(settings.commands.len(), 1);
        assert_eq!(settings.commands[0].name, "plain");
        assert_eq!(settings.diagnostics.len(), 1);
        assert!(settings.diagnostics[0].contains("broken.md"));
    }

    #[test]
    fn malformed_frontmatter_is_reported_without_path_escape() {
        assert!(parse_command("review", "---\ndescription: \"bad\n---\nprompt").is_err());
        assert!(command_path(Path::new("/tmp"), "../escape").is_err());
    }
}
