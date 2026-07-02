use agent_protocol::{PermissionMode, PermissionProfile, ShellPolicy};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_AUTO_COMPACT: bool = true;
const DEFAULT_MAX_CONTEXT_CHARS: usize = 64_000;
const DEFAULT_RETAIN_RECENT_TURNS: usize = 6;
const DEFAULT_SUMMARY_TARGET_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub permissions: PermissionProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub system_prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextConfig {
    pub auto_compact: bool,
    pub max_context_chars: usize,
    pub retain_recent_turns: usize,
    pub summary_target_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub path: PathBuf,
    pub api_key: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    ExplicitConfigNotFound { path: PathBuf },
    #[error("no config file found; searched: {searched}")]
    NoConfigFile { searched: String },
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("missing required config value: [model].model")]
    MissingModel,
    #[error("configured API key environment variable {env_var} is not set")]
    MissingApiKey { env_var: String },
    #[error("invalid context config value: [context].{field} must be greater than 0")]
    InvalidContextValue { field: &'static str },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAppConfig {
    model: Option<RawModelConfig>,
    agent: Option<RawAgentConfig>,
    context: Option<RawContextConfig>,
    permissions: Option<RawPermissionsConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelConfig {
    base_url: Option<String>,
    model: Option<String>,
    api_key_env: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentConfig {
    system_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContextConfig {
    auto_compact: Option<bool>,
    max_context_chars: Option<usize>,
    retain_recent_turns: Option<usize>,
    summary_target_chars: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionsConfig {
    mode: Option<PermissionMode>,
    shell: Option<ShellPolicy>,
}

pub fn load_config(explicit_path: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_config_from_locations(explicit_path, &cwd, dirs::home_dir().as_deref())
}

fn load_config_from_locations(
    explicit_path: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<LoadedConfig, ConfigError> {
    let path = select_config_path(explicit_path, cwd, home)?;
    let content = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let raw = toml::from_str::<RawAppConfig>(&content).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    let inline_api_key = raw
        .model
        .as_ref()
        .and_then(|model| model.openai_api_key.as_deref())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string);
    let config = AppConfig::try_from(raw)?;
    let api_key = match inline_api_key {
        Some(api_key) => api_key,
        None => env::var(&config.model.api_key_env).map_err(|_| ConfigError::MissingApiKey {
            env_var: config.model.api_key_env.clone(),
        })?,
    };

    Ok(LoadedConfig {
        config,
        path,
        api_key,
    })
}

fn select_config_path(
    explicit_path: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(ConfigError::ExplicitConfigNotFound {
            path: path.to_path_buf(),
        });
    }

    let local = cwd.join("morrow.toml");
    if local.is_file() {
        return Ok(local);
    }

    let user = home.map(|home| home.join(".morrow").join("config.toml"));
    if let Some(path) = user.as_ref()
        && path.is_file()
    {
        return Ok(path.clone());
    }

    let mut searched = vec![local.display().to_string()];
    if let Some(path) = user {
        searched.push(path.display().to_string());
    }

    Err(ConfigError::NoConfigFile {
        searched: searched.join(", "),
    })
}

impl TryFrom<RawAppConfig> for AppConfig {
    type Error = ConfigError;

    fn try_from(value: RawAppConfig) -> Result<Self, Self::Error> {
        let model = value.model.unwrap_or_default();
        let model_name = model
            .model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .ok_or(ConfigError::MissingModel)?;

        let agent = value.agent.unwrap_or_default();
        let context = ContextConfig::try_from(value.context.unwrap_or_default())?;
        let permissions = value.permissions.unwrap_or_default();
        let mode = permissions.mode.unwrap_or_default();
        let mut permissions_profile = PermissionProfile::for_mode(mode);
        if let Some(shell) = permissions.shell {
            permissions_profile.shell = shell;
        }

        Ok(Self {
            model: ModelConfig {
                base_url: model
                    .base_url
                    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
                model: model_name,
                api_key_env: model
                    .api_key_env
                    .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string()),
                timeout_secs: model.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            },
            agent: AgentConfig {
                system_prompt: agent
                    .system_prompt
                    .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
            },
            context,
            permissions: permissions_profile,
        })
    }
}

impl TryFrom<RawContextConfig> for ContextConfig {
    type Error = ConfigError;

    fn try_from(value: RawContextConfig) -> Result<Self, Self::Error> {
        let max_context_chars = non_zero_context_value(
            "max_context_chars",
            value.max_context_chars.unwrap_or(DEFAULT_MAX_CONTEXT_CHARS),
        )?;
        let retain_recent_turns = non_zero_context_value(
            "retain_recent_turns",
            value
                .retain_recent_turns
                .unwrap_or(DEFAULT_RETAIN_RECENT_TURNS),
        )?;
        let summary_target_chars = non_zero_context_value(
            "summary_target_chars",
            value
                .summary_target_chars
                .unwrap_or(DEFAULT_SUMMARY_TARGET_CHARS),
        )?;

        Ok(Self {
            auto_compact: value.auto_compact.unwrap_or(DEFAULT_AUTO_COMPACT),
            max_context_chars,
            retain_recent_turns,
            summary_target_chars,
        })
    }
}

fn non_zero_context_value(field: &'static str, value: usize) -> Result<usize, ConfigError> {
    if value == 0 {
        return Err(ConfigError::InvalidContextValue { field });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = env::temp_dir().join(format!("morrow-{name}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_config(path: &Path, model: &str, api_key_env: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create config parent");
        }
        fs::write(
            path,
            format!(
                r#"
[model]
model = "{model}"
api_key_env = "{api_key_env}"
"#
            ),
        )
        .expect("write config");
    }

    fn write_inline_key_config(path: &Path, model: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create config parent");
        }
        fs::write(
            path,
            format!(
                r#"
[model]
model = "{model}"
OPENAI_API_KEY = "inline-secret"
"#
            ),
        )
        .expect("write config");
    }

    fn write_permissions_config(path: &Path, model: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create config parent");
        }
        fs::write(
            path,
            format!(
                r#"
[model]
model = "{model}"
api_key_env = "MORROW_PERMISSIONS_KEY"

[permissions]
mode = "workspace_write"
shell = "deny"
"#
            ),
        )
        .expect("write config");
    }

    fn write_context_config(path: &Path, model: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create config parent");
        }
        fs::write(
            path,
            format!(
                r#"
[model]
model = "{model}"
api_key_env = "MORROW_CONTEXT_KEY"

[context]
auto_compact = false
max_context_chars = 1024
retain_recent_turns = 2
summary_target_chars = 256
"#
            ),
        )
        .expect("write config");
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: These tests use unique environment variable names and do not
        // read them concurrently from other test threads in this crate.
        unsafe {
            env::set_var(key, value);
        }
    }

    #[test]
    fn explicit_config_path_takes_priority() {
        let root = unique_dir("explicit-priority");
        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(&home).expect("create home");

        let explicit = root.join("explicit.toml");
        write_config(&cwd.join("morrow.toml"), "local-model", "MORROW_LOCAL_KEY");
        write_config(&explicit, "explicit-model", "MORROW_EXPLICIT_KEY");
        set_env("MORROW_EXPLICIT_KEY", "secret");

        let loaded =
            load_config_from_locations(Some(&explicit), &cwd, Some(&home)).expect("load config");

        assert_eq!(loaded.path, explicit);
        assert_eq!(loaded.config.model.model, "explicit-model");
        assert_eq!(loaded.api_key, "secret");
    }

    #[test]
    fn local_config_takes_priority_over_home_config() {
        let root = unique_dir("local-priority");
        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("create cwd");

        write_config(
            &cwd.join("morrow.toml"),
            "local-model",
            "MORROW_LOCAL_PRIORITY_KEY",
        );
        write_config(
            &home.join(".morrow").join("config.toml"),
            "home-model",
            "MORROW_HOME_PRIORITY_KEY",
        );
        set_env("MORROW_LOCAL_PRIORITY_KEY", "local-secret");

        let loaded =
            load_config_from_locations(None, &cwd, Some(&home)).expect("load local config");

        assert_eq!(loaded.config.model.model, "local-model");
        assert_eq!(loaded.api_key, "local-secret");
    }

    #[test]
    fn missing_model_is_rejected() {
        let root = unique_dir("missing-model");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            "[model]\napi_key_env = \"MORROW_MISSING_MODEL_KEY\"\n",
        )
        .expect("write config");
        set_env("MORROW_MISSING_MODEL_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(err, ConfigError::MissingModel));
    }

    #[test]
    fn missing_api_key_env_is_rejected() {
        let root = unique_dir("missing-api-key");
        let config = root.join("morrow.toml");
        write_config(&config, "test-model", "MORROW_MISSING_API_KEY_VALUE");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::MissingApiKey { env_var } if env_var == "MORROW_MISSING_API_KEY_VALUE"
        ));
    }

    #[test]
    fn inline_openai_api_key_is_supported() {
        let root = unique_dir("inline-api-key");
        let config = root.join("morrow.toml");
        write_inline_key_config(&config, "test-model");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        assert_eq!(loaded.config.model.model, "test-model");
        assert_eq!(loaded.api_key, "inline-secret");
    }

    #[test]
    fn defaults_optional_config_values() {
        let root = unique_dir("defaults");
        let config = root.join("morrow.toml");
        write_config(&config, "test-model", "MORROW_DEFAULTS_KEY");
        set_env("MORROW_DEFAULTS_KEY", "secret");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        assert_eq!(loaded.config.model.base_url, DEFAULT_BASE_URL);
        assert_eq!(loaded.config.model.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(loaded.config.agent.system_prompt, DEFAULT_SYSTEM_PROMPT);
        assert_eq!(
            loaded.config.context,
            ContextConfig {
                auto_compact: DEFAULT_AUTO_COMPACT,
                max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
                retain_recent_turns: DEFAULT_RETAIN_RECENT_TURNS,
                summary_target_chars: DEFAULT_SUMMARY_TARGET_CHARS,
            }
        );
        assert_eq!(
            loaded.config.permissions,
            PermissionProfile::for_mode(PermissionMode::ReadOnly)
        );
    }

    #[test]
    fn loads_permissions_config() {
        let root = unique_dir("permissions");
        let config = root.join("morrow.toml");
        write_permissions_config(&config, "test-model");
        set_env("MORROW_PERMISSIONS_KEY", "secret");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        assert_eq!(
            loaded.config.permissions,
            PermissionProfile {
                mode: PermissionMode::WorkspaceWrite,
                shell: ShellPolicy::Deny,
            }
        );
    }

    #[test]
    fn loads_context_config() {
        let root = unique_dir("context");
        let config = root.join("morrow.toml");
        write_context_config(&config, "test-model");
        set_env("MORROW_CONTEXT_KEY", "secret");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        assert_eq!(
            loaded.config.context,
            ContextConfig {
                auto_compact: false,
                max_context_chars: 1024,
                retain_recent_turns: 2,
                summary_target_chars: 256,
            }
        );
    }

    #[test]
    fn rejects_zero_context_values() {
        let root = unique_dir("context-zero");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_CONTEXT_ZERO_KEY"

[context]
max_context_chars = 0
"#,
        )
        .expect("write config");
        set_env("MORROW_CONTEXT_ZERO_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::InvalidContextValue {
                field: "max_context_chars"
            }
        ));
    }
}
