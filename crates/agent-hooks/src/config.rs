use crate::types::{
    HOOK_CONFIG_SCHEMA_VERSION, HookConfigSource, HookDefinition, HookDefinitionStatus, HookError,
    HookSettings, MAX_HOOK_TIMEOUT_SECS, MIN_HOOK_TIMEOUT_SECS,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookConfigFile {
    schema_version: u32,
    #[serde(default)]
    hooks: Vec<HookDefinition>,
}

#[derive(Debug)]
pub(crate) struct LoadedHooks {
    pub user_hooks: Vec<HookDefinition>,
    pub project_hooks: Vec<HookDefinition>,
    pub project_fingerprint: Option<String>,
    pub project_trusted: bool,
}

impl LoadedHooks {
    pub fn settings(
        &self,
        user_config_path: &Path,
        project_config_path: &Path,
        trust_store_path: &Path,
    ) -> HookSettings {
        let mut hooks = self
            .user_hooks
            .iter()
            .map(|hook| hook_status(hook, HookConfigSource::User, true))
            .collect::<Vec<_>>();
        hooks.extend(
            self.project_hooks
                .iter()
                .map(|hook| hook_status(hook, HookConfigSource::Project, self.project_trusted)),
        );
        let diagnostics = if self.project_fingerprint.is_some() && !self.project_trusted {
            vec![
                "Project hooks are disabled until this exact configuration is trusted. They can execute repository-controlled commands with the full host environment, including API keys."
                    .to_string(),
            ]
        } else {
            Vec::new()
        };
        HookSettings {
            schema_version: HOOK_CONFIG_SCHEMA_VERSION,
            user_config_path: user_config_path.to_string_lossy().into_owned(),
            project_config_path: project_config_path.to_string_lossy().into_owned(),
            trust_store_path: trust_store_path.to_string_lossy().into_owned(),
            project_fingerprint: self.project_fingerprint.clone(),
            project_trusted: self.project_trusted,
            hooks,
            diagnostics,
        }
    }
}

pub(crate) fn load_hook_file(path: &Path) -> Result<Option<Vec<HookDefinition>>, HookError> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|source| HookError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config =
        toml::from_str::<HookConfigFile>(&content).map_err(|source| HookError::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
    if config.schema_version != HOOK_CONFIG_SCHEMA_VERSION {
        return Err(HookError::UnsupportedConfigSchema {
            path: path.to_path_buf(),
            version: config.schema_version,
        });
    }
    validate_hooks(path, &config.hooks)?;
    Ok(Some(config.hooks))
}

fn hook_status(
    hook: &HookDefinition,
    source: HookConfigSource,
    trusted: bool,
) -> HookDefinitionStatus {
    HookDefinitionStatus {
        id: hook.id.clone(),
        event: hook.event,
        command: hook.command.clone(),
        timeout_secs: hook.timeout_secs,
        failure_mode: hook.failure_mode,
        tool_names: hook.tool_names.clone(),
        agent_scopes: hook.agent_scopes.clone(),
        source,
        trusted,
        active: trusted,
    }
}

fn validate_hooks(path: &Path, hooks: &[HookDefinition]) -> Result<(), HookError> {
    let mut ids = HashSet::new();
    for hook in hooks {
        if hook.id.trim().is_empty() {
            return invalid_config(path, "hook id must not be empty");
        }
        if !ids.insert(hook.id.as_str()) {
            return invalid_config(path, format!("duplicate hook id {:?}", hook.id));
        }
        if hook
            .command
            .first()
            .is_none_or(|command| command.trim().is_empty())
        {
            return invalid_config(
                path,
                format!("hook {:?} command must not be empty", hook.id),
            );
        }
        if !(MIN_HOOK_TIMEOUT_SECS..=MAX_HOOK_TIMEOUT_SECS).contains(&hook.timeout_secs) {
            return invalid_config(
                path,
                format!(
                    "hook {:?} timeout_secs must be between {MIN_HOOK_TIMEOUT_SECS} and {MAX_HOOK_TIMEOUT_SECS}",
                    hook.id
                ),
            );
        }
        if hook.tool_names.is_some() && !hook.event.is_tool_event() {
            return invalid_config(
                path,
                format!(
                    "hook {:?} tool_names is only valid for tool events",
                    hook.id
                ),
            );
        }
        validate_exact_list(path, hook, "tool_names", hook.tool_names.as_deref())?;
        if let Some(scopes) = hook.agent_scopes.as_deref()
            && (scopes.is_empty() || scopes.iter().collect::<HashSet<_>>().len() != scopes.len())
        {
            return invalid_config(
                path,
                format!(
                    "hook {:?} agent_scopes must be a non-empty unique list",
                    hook.id
                ),
            );
        }
    }
    Ok(())
}

fn validate_exact_list(
    path: &Path,
    hook: &HookDefinition,
    field: &str,
    values: Option<&[String]>,
) -> Result<(), HookError> {
    if let Some(values) = values
        && (values.is_empty()
            || values.iter().any(|value| value.trim().is_empty())
            || values.iter().collect::<HashSet<_>>().len() != values.len())
    {
        return invalid_config(
            path,
            format!("hook {:?} {field} must be a non-empty unique list", hook.id),
        );
    }
    Ok(())
}

fn invalid_config<T>(path: &Path, message: impl Into<String>) -> Result<T, HookError> {
    Err(HookError::InvalidConfig {
        path: path.to_path_buf(),
        message: message.into(),
    })
}
