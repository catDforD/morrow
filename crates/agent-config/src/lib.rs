use agent_protocol::{PermissionMode, PermissionProfile, ShellPolicy};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_RESERVED_OUTPUT_TOKENS: usize = 8_192;
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_AUTO_COMPACT: bool = true;
const DEFAULT_AUTO_COMPACT_THRESHOLD: f32 = 0.835;
const DEFAULT_RETAIN_RECENT_TURNS: usize = 6;
const DEFAULT_SUMMARY_TARGET_TOKENS: usize = 12_000;
const DEFAULT_COMPACT_MAX_RETRIES: usize = 2;
const DEFAULT_MCP_STARTUP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MCP_TOOL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub permissions: PermissionProfile,
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_secs: u64,
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelConfig")
            .field("base_url", &"<configured>")
            .field("model", &self.model)
            .field("api_key_env", &self.api_key_env)
            .field("timeout_secs", &self.timeout_secs)
            .field("context_window_tokens", &self.context_window_tokens)
            .field("reserved_output_tokens", &self.reserved_output_tokens)
            .finish()
    }
}

impl ModelConfig {
    pub fn context_limits(&self) -> ModelContextLimits {
        ModelContextLimits {
            context_window_tokens: self.context_window_tokens,
            reserved_output_tokens: self.reserved_output_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelContextLimits {
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub system_prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextConfig {
    pub auto_compact: bool,
    pub auto_compact_threshold: f32,
    pub retain_recent_turns: usize,
    pub summary_target_tokens: usize,
    pub compact_max_retries: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &format_args!("<{} entries>", self.args.len()))
            .field(
                "env",
                &self.env.keys().map(String::as_str).collect::<Vec<_>>(),
            )
            .field("cwd", &self.cwd)
            .field("url", &self.url.as_ref().map(|_| "<configured>"))
            .field(
                "http_headers",
                &self
                    .http_headers
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .field("enabled", &self.enabled)
            .field("startup_timeout_sec", &self.startup_timeout_sec)
            .field("tool_timeout_sec", &self.tool_timeout_sec)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub path: PathBuf,
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerAppConfig {
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub permissions: PermissionProfile,
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LoadedServerModel {
    pub config: ModelConfig,
    pub api_key: String,
}

impl std::fmt::Debug for LoadedServerModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedServerModel")
            .field("config", &self.config)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedServerConfig {
    pub config: ServerAppConfig,
    pub path: Option<PathBuf>,
    pub model: Option<LoadedServerModel>,
    pub diagnostics: Vec<String>,
}

impl std::fmt::Debug for LoadedConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedConfig")
            .field("config", &self.config)
            .field("path", &self.path)
            .field("api_key", &"<redacted>")
            .finish()
    }
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
    #[error("missing required config value: [model].context_window_tokens")]
    MissingContextWindowTokens,
    #[error("configured API key environment variable {env_var} is not set")]
    MissingApiKey { env_var: String },
    #[error("invalid config value: {field} must be greater than 0")]
    InvalidPositiveValue { field: &'static str },
    #[error(
        "invalid config value: [context].auto_compact_threshold must be greater than 0 and less than or equal to 1"
    )]
    InvalidAutoCompactThreshold,
    #[error("invalid config value: [mcp_servers.{server}].{field} must be greater than 0")]
    InvalidMcpPositiveValue { server: String, field: &'static str },
    #[error("missing required config value: [mcp_servers.{server}].command")]
    MissingMcpCommand { server: String },
    #[error("missing required config value: [mcp_servers.{server}].url")]
    MissingMcpUrl { server: String },
    #[error(
        "configured MCP environment variable {env_var} for [mcp_servers.{server}].{field} is not set"
    )]
    MissingMcpEnvVar {
        server: String,
        field: String,
        env_var: String,
    },
    #[error("unsupported MCP config value: [mcp_servers.{server}].{field}")]
    UnsupportedMcpField { server: String, field: &'static str },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAppConfig {
    model: Option<RawModelConfig>,
    agent: Option<RawAgentConfig>,
    context: Option<RawContextConfig>,
    permissions: Option<RawPermissionsConfig>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawMcpServerConfig>,
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
    context_window_tokens: Option<usize>,
    reserved_output_tokens: Option<usize>,
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
    auto_compact_threshold: Option<f32>,
    retain_recent_turns: Option<usize>,
    summary_target_tokens: Option<usize>,
    compact_max_retries: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionsConfig {
    mode: Option<PermissionMode>,
    shell: Option<ShellPolicy>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServerConfig {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    enabled: Option<bool>,
    startup_timeout_sec: Option<u64>,
    tool_timeout_sec: Option<u64>,
    url: Option<String>,
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    #[serde(default)]
    env_http_headers: BTreeMap<String, String>,
    oauth_client_id: Option<String>,
    oauth_resource: Option<String>,
}

pub fn load_config(explicit_path: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_config_from_locations(explicit_path, &cwd, dirs::home_dir().as_deref())
}

pub fn load_server_config(explicit_path: Option<&Path>) -> Result<LoadedServerConfig, ConfigError> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_server_config_for_workspace(explicit_path, &cwd)
}

pub fn load_server_config_for_workspace(
    explicit_path: Option<&Path>,
    workspace: &Path,
) -> Result<LoadedServerConfig, ConfigError> {
    load_server_config_from_locations(explicit_path, workspace, dirs::home_dir().as_deref())
}

fn load_server_config_from_locations(
    explicit_path: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<LoadedServerConfig, ConfigError> {
    let path = select_optional_config_path(explicit_path, cwd, home)?;
    let raw = match path.as_ref() {
        Some(path) => {
            let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            toml::from_str::<RawAppConfig>(&content).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?
        }
        None => RawAppConfig::default(),
    };

    let RawAppConfig {
        model,
        agent,
        context,
        permissions,
        mcp_servers,
    } = raw;
    let config = parse_server_app_config(agent, context, permissions, mcp_servers)?;
    let mut diagnostics = Vec::new();
    let model = model.and_then(|model| match parse_model_config(model) {
        Ok((config, inline_api_key)) => {
            let api_key = inline_api_key.or_else(|| env::var(&config.api_key_env).ok());
            match api_key.filter(|key| !key.trim().is_empty()) {
                Some(api_key) => Some(LoadedServerModel { config, api_key }),
                None => {
                    diagnostics.push(format!(
                        "configured model is unavailable because API key environment variable {} is not set",
                        config.api_key_env
                    ));
                    None
                }
            }
        }
        Err(error) => {
            diagnostics.push(format!("configured model is unavailable: {error}"));
            None
        }
    });
    if model.is_none() && diagnostics.is_empty() {
        diagnostics.push("no model is configured; add one in Web settings".to_string());
    }

    Ok(LoadedServerConfig {
        config,
        path,
        model,
        diagnostics,
    })
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

fn select_optional_config_path(
    explicit_path: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    if explicit_path.is_some() {
        return select_config_path(explicit_path, cwd, home).map(Some);
    }

    let local = cwd.join("morrow.toml");
    if local.is_file() {
        return Ok(Some(local));
    }
    let user = home.map(|home| home.join(".morrow").join("config.toml"));
    Ok(user.filter(|path| path.is_file()))
}

impl TryFrom<RawAppConfig> for AppConfig {
    type Error = ConfigError;

    fn try_from(value: RawAppConfig) -> Result<Self, Self::Error> {
        let RawAppConfig {
            model,
            agent,
            context,
            permissions,
            mcp_servers,
        } = value;
        let (model, _) = parse_model_config(model.unwrap_or_default())?;
        let server = parse_server_app_config(agent, context, permissions, mcp_servers)?;

        Ok(Self {
            model,
            agent: server.agent,
            context: server.context,
            permissions: server.permissions,
            mcp_servers: server.mcp_servers,
        })
    }
}

fn parse_model_config(model: RawModelConfig) -> Result<(ModelConfig, Option<String>), ConfigError> {
    let inline_api_key = model
        .openai_api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string);
    let model_name = model
        .model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .ok_or(ConfigError::MissingModel)?;
    let context_window_tokens = positive_config_value(
        "[model].context_window_tokens",
        model
            .context_window_tokens
            .ok_or(ConfigError::MissingContextWindowTokens)?,
    )?;
    let reserved_output_tokens = positive_config_value(
        "[model].reserved_output_tokens",
        model
            .reserved_output_tokens
            .unwrap_or(DEFAULT_RESERVED_OUTPUT_TOKENS),
    )?;

    Ok((
        ModelConfig {
            base_url: model
                .base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: model_name,
            api_key_env: model
                .api_key_env
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string()),
            timeout_secs: model.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            context_window_tokens,
            reserved_output_tokens,
        },
        inline_api_key,
    ))
}

fn parse_server_app_config(
    agent: Option<RawAgentConfig>,
    context: Option<RawContextConfig>,
    permissions: Option<RawPermissionsConfig>,
    mcp_servers: BTreeMap<String, RawMcpServerConfig>,
) -> Result<ServerAppConfig, ConfigError> {
    let agent = agent.unwrap_or_default();
    let context = ContextConfig::try_from(context.unwrap_or_default())?;
    let permissions = permissions.unwrap_or_default();
    let mode = permissions.mode.unwrap_or_default();
    let mut permissions_profile = PermissionProfile::for_mode(mode);
    if let Some(shell) = permissions.shell {
        permissions_profile.shell = shell;
    }

    Ok(ServerAppConfig {
        agent: AgentConfig {
            system_prompt: agent
                .system_prompt
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        },
        context,
        permissions: permissions_profile,
        mcp_servers: parse_mcp_servers(mcp_servers)?,
    })
}

impl TryFrom<RawContextConfig> for ContextConfig {
    type Error = ConfigError;

    fn try_from(value: RawContextConfig) -> Result<Self, Self::Error> {
        let auto_compact_threshold = value
            .auto_compact_threshold
            .unwrap_or(DEFAULT_AUTO_COMPACT_THRESHOLD);
        if !auto_compact_threshold.is_finite()
            || auto_compact_threshold <= 0.0
            || auto_compact_threshold > 1.0
        {
            return Err(ConfigError::InvalidAutoCompactThreshold);
        }

        let retain_recent_turns = positive_config_value(
            "[context].retain_recent_turns",
            value
                .retain_recent_turns
                .unwrap_or(DEFAULT_RETAIN_RECENT_TURNS),
        )?;
        let summary_target_tokens = positive_config_value(
            "[context].summary_target_tokens",
            value
                .summary_target_tokens
                .unwrap_or(DEFAULT_SUMMARY_TARGET_TOKENS),
        )?;
        let compact_max_retries = positive_config_value(
            "[context].compact_max_retries",
            value
                .compact_max_retries
                .unwrap_or(DEFAULT_COMPACT_MAX_RETRIES),
        )?;

        Ok(Self {
            auto_compact: value.auto_compact.unwrap_or(DEFAULT_AUTO_COMPACT),
            auto_compact_threshold,
            retain_recent_turns,
            summary_target_tokens,
            compact_max_retries,
        })
    }
}

fn positive_config_value(field: &'static str, value: usize) -> Result<usize, ConfigError> {
    if value == 0 {
        return Err(ConfigError::InvalidPositiveValue { field });
    }
    Ok(value)
}

fn parse_mcp_servers(
    raw_servers: BTreeMap<String, RawMcpServerConfig>,
) -> Result<Vec<McpServerConfig>, ConfigError> {
    let mut servers = Vec::with_capacity(raw_servers.len());
    for (name, raw) in raw_servers {
        if raw.oauth_client_id.is_some() {
            return Err(ConfigError::UnsupportedMcpField {
                server: name,
                field: "oauth_client_id",
            });
        }
        if raw.oauth_resource.is_some() {
            return Err(ConfigError::UnsupportedMcpField {
                server: name,
                field: "oauth_resource",
            });
        }

        let startup_timeout_sec = raw
            .startup_timeout_sec
            .unwrap_or(DEFAULT_MCP_STARTUP_TIMEOUT_SECS);
        if startup_timeout_sec == 0 {
            return Err(ConfigError::InvalidMcpPositiveValue {
                server: name.clone(),
                field: "startup_timeout_sec",
            });
        }
        let tool_timeout_sec = raw
            .tool_timeout_sec
            .unwrap_or(DEFAULT_MCP_TOOL_TIMEOUT_SECS);
        if tool_timeout_sec == 0 {
            return Err(ConfigError::InvalidMcpPositiveValue {
                server: name.clone(),
                field: "tool_timeout_sec",
            });
        }

        let transport = if raw.url.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        let enabled = raw.enabled.unwrap_or(true);

        match transport {
            McpTransport::Stdio => {
                if raw.bearer_token_env_var.is_some() {
                    return Err(ConfigError::UnsupportedMcpField {
                        server: name,
                        field: "bearer_token_env_var",
                    });
                }
                if !raw.http_headers.is_empty() {
                    return Err(ConfigError::UnsupportedMcpField {
                        server: name,
                        field: "http_headers",
                    });
                }
                if !raw.env_http_headers.is_empty() {
                    return Err(ConfigError::UnsupportedMcpField {
                        server: name,
                        field: "env_http_headers",
                    });
                }

                let command = raw
                    .command
                    .map(|command| command.trim().to_string())
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| ConfigError::MissingMcpCommand {
                        server: name.clone(),
                    })?;

                servers.push(McpServerConfig {
                    name,
                    transport,
                    command,
                    args: raw.args,
                    env: raw.env,
                    cwd: raw.cwd.map(PathBuf::from),
                    url: None,
                    http_headers: BTreeMap::new(),
                    enabled,
                    startup_timeout_sec,
                    tool_timeout_sec,
                });
            }
            McpTransport::Http => {
                let url = raw
                    .url
                    .map(|url| url.trim().to_string())
                    .filter(|url| !url.is_empty())
                    .ok_or_else(|| ConfigError::MissingMcpUrl {
                        server: name.clone(),
                    })?;
                let mut http_headers = raw.http_headers;
                for (header, env_var) in raw.env_http_headers {
                    let value = env::var(&env_var).map_err(|_| ConfigError::MissingMcpEnvVar {
                        server: name.clone(),
                        field: format!("env_http_headers.{header}"),
                        env_var: env_var.clone(),
                    })?;
                    http_headers.insert(header, value);
                }
                if let Some(env_var) = raw.bearer_token_env_var {
                    let token = env::var(&env_var)
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| ConfigError::MissingMcpEnvVar {
                            server: name.clone(),
                            field: "bearer_token_env_var".to_string(),
                            env_var: env_var.clone(),
                        })?;
                    http_headers.insert("Authorization".to_string(), format!("Bearer {token}"));
                }

                servers.push(McpServerConfig {
                    name,
                    transport,
                    command: String::new(),
                    args: Vec::new(),
                    env: raw.env,
                    cwd: None,
                    url: Some(url),
                    http_headers,
                    enabled,
                    startup_timeout_sec,
                    tool_timeout_sec,
                });
            }
        }
    }
    Ok(servers)
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
context_window_tokens = 65536
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
context_window_tokens = 65536
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
context_window_tokens = 65536

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
context_window_tokens = 131072
reserved_output_tokens = 4096

[context]
auto_compact = false
auto_compact_threshold = 0.75
retain_recent_turns = 2
summary_target_tokens = 256
compact_max_retries = 3
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
            "[model]\napi_key_env = \"MORROW_MISSING_MODEL_KEY\"\ncontext_window_tokens = 65536\n",
        )
        .expect("write config");
        set_env("MORROW_MISSING_MODEL_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(err, ConfigError::MissingModel));
    }

    #[test]
    fn server_config_allows_missing_file_and_uses_common_defaults() {
        let cwd = unique_dir("server-no-config-cwd");
        let home = unique_dir("server-no-config-home");

        let loaded =
            load_server_config_from_locations(None, &cwd, Some(&home)).expect("server config");

        assert_eq!(loaded.path, None);
        assert_eq!(loaded.model, None);
        assert_eq!(loaded.config.agent.system_prompt, DEFAULT_SYSTEM_PROMPT);
        assert!(loaded.config.mcp_servers.is_empty());
        assert!(loaded.diagnostics[0].contains("no model"));
    }

    #[test]
    fn server_config_keeps_running_when_model_is_incomplete() {
        let cwd = unique_dir("server-incomplete-cwd");
        let home = unique_dir("server-incomplete-home");
        let config = cwd.join("morrow.toml");
        fs::write(
            &config,
            r#"
[agent]
system_prompt = "Web bootstrap"

[model]
model = "deepseek-v4-pro"
"#,
        )
        .expect("write config");

        let loaded =
            load_server_config_from_locations(None, &cwd, Some(&home)).expect("server config");

        assert_eq!(loaded.path.as_deref(), Some(config.as_path()));
        assert_eq!(loaded.model, None);
        assert_eq!(loaded.config.agent.system_prompt, "Web bootstrap");
        assert!(loaded.diagnostics[0].contains("context_window_tokens"));
    }

    #[test]
    fn server_config_can_be_loaded_for_an_explicit_workspace() {
        let workspace = unique_dir("server-explicit-workspace");
        let config = workspace.join("morrow.toml");
        fs::write(
            &config,
            r#"
[agent]
system_prompt = "Desktop workspace"
"#,
        )
        .expect("write config");

        let loaded = load_server_config_for_workspace(None, &workspace).expect("workspace config");

        assert_eq!(loaded.path.as_deref(), Some(config.as_path()));
        assert_eq!(loaded.config.agent.system_prompt, "Desktop workspace");
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
        assert_eq!(loaded.config.model.context_window_tokens, 65_536);
        assert_eq!(
            loaded.config.model.reserved_output_tokens,
            DEFAULT_RESERVED_OUTPUT_TOKENS
        );
        assert_eq!(loaded.config.agent.system_prompt, DEFAULT_SYSTEM_PROMPT);
        assert_eq!(
            loaded.config.context,
            ContextConfig {
                auto_compact: DEFAULT_AUTO_COMPACT,
                auto_compact_threshold: DEFAULT_AUTO_COMPACT_THRESHOLD,
                retain_recent_turns: DEFAULT_RETAIN_RECENT_TURNS,
                summary_target_tokens: DEFAULT_SUMMARY_TARGET_TOKENS,
                compact_max_retries: DEFAULT_COMPACT_MAX_RETRIES,
            }
        );
        assert_eq!(
            loaded.config.permissions,
            PermissionProfile::for_mode(PermissionMode::ReadOnly)
        );
        assert!(loaded.config.mcp_servers.is_empty());
    }

    #[test]
    fn loads_mcp_stdio_server_config() {
        let root = unique_dir("mcp-stdio");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_KEY"
context_window_tokens = 65536

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { FOO = "bar" }
cwd = "."
startup_timeout_sec = 11
tool_timeout_sec = 22
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_KEY", "secret");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        assert_eq!(loaded.config.mcp_servers.len(), 1);
        let server = &loaded.config.mcp_servers[0];
        assert_eq!(server.name, "filesystem");
        assert_eq!(server.transport, McpTransport::Stdio);
        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args,
            ["-y", "@modelcontextprotocol/server-filesystem", "."]
        );
        assert_eq!(server.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(server.cwd.as_deref(), Some(Path::new(".")));
        assert_eq!(server.url, None);
        assert!(server.http_headers.is_empty());
        assert!(server.enabled);
        assert_eq!(server.startup_timeout_sec, 11);
        assert_eq!(server.tool_timeout_sec, 22);
    }

    #[test]
    fn loads_disabled_mcp_server_config_with_defaults() {
        let root = unique_dir("mcp-disabled");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_DISABLED_KEY"
context_window_tokens = 65536

[mcp_servers.docs]
command = "docs-mcp"
enabled = false
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_DISABLED_KEY", "secret");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        let server = &loaded.config.mcp_servers[0];
        assert!(!server.enabled);
        assert_eq!(server.startup_timeout_sec, DEFAULT_MCP_STARTUP_TIMEOUT_SECS);
        assert_eq!(server.tool_timeout_sec, DEFAULT_MCP_TOOL_TIMEOUT_SECS);
    }

    #[test]
    fn rejects_mcp_server_without_command() {
        let root = unique_dir("mcp-missing-command");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_MISSING_KEY"
context_window_tokens = 65536

[mcp_servers.bad]
args = ["--serve"]
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_MISSING_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::MissingMcpCommand { server } if server == "bad"
        ));
    }

    #[test]
    fn rejects_invalid_mcp_timeout() {
        let root = unique_dir("mcp-timeout");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_TIMEOUT_KEY"
context_window_tokens = 65536

[mcp_servers.bad]
command = "mcp"
tool_timeout_sec = 0
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_TIMEOUT_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::InvalidMcpPositiveValue { server, field }
                if server == "bad" && field == "tool_timeout_sec"
        ));
    }

    #[test]
    fn loads_http_mcp_server_config_without_command() {
        let root = unique_dir("mcp-http");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_HTTP_KEY"
context_window_tokens = 65536

[mcp_servers.remote]
url = "https://example.com/mcp"
http_headers = { "X-Morrow" = "static" }
env_http_headers = { "X-Env" = "MORROW_MCP_HTTP_HEADER" }
bearer_token_env_var = "MORROW_MCP_HTTP_TOKEN"
startup_timeout_sec = 12
tool_timeout_sec = 34
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_HTTP_KEY", "secret");
        set_env("MORROW_MCP_HTTP_HEADER", "from-env");
        set_env("MORROW_MCP_HTTP_TOKEN", "token");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

        let server = &loaded.config.mcp_servers[0];
        assert_eq!(server.name, "remote");
        assert_eq!(server.transport, McpTransport::Http);
        assert_eq!(server.command, "");
        assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            server.http_headers.get("X-Morrow").map(String::as_str),
            Some("static")
        );
        assert_eq!(
            server.http_headers.get("X-Env").map(String::as_str),
            Some("from-env")
        );
        assert_eq!(
            server.http_headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
        assert_eq!(server.startup_timeout_sec, 12);
        assert_eq!(server.tool_timeout_sec, 34);
    }

    #[test]
    fn rejects_missing_mcp_http_env_header() {
        let root = unique_dir("mcp-http-missing-env");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_HTTP_MISSING_ENV_KEY"
context_window_tokens = 65536

[mcp_servers.remote]
url = "https://example.com/mcp"
env_http_headers = { "X-Env" = "MORROW_MCP_HTTP_DOES_NOT_EXIST" }
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_HTTP_MISSING_ENV_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::MissingMcpEnvVar { server, field, env_var }
                if server == "remote"
                    && field == "env_http_headers.X-Env"
                    && env_var == "MORROW_MCP_HTTP_DOES_NOT_EXIST"
        ));
    }

    #[test]
    fn rejects_oauth_mcp_config_for_http_v1() {
        let root = unique_dir("mcp-http-oauth");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MCP_HTTP_OAUTH_KEY"
context_window_tokens = 65536

[mcp_servers.remote]
url = "https://example.com/mcp"
oauth_client_id = "client"
"#,
        )
        .expect("write config");
        set_env("MORROW_MCP_HTTP_OAUTH_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::UnsupportedMcpField { server, field }
                if server == "remote" && field == "oauth_client_id"
        ));
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
                auto_compact_threshold: 0.75,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 3,
            }
        );
        assert_eq!(loaded.config.model.context_window_tokens, 131_072);
        assert_eq!(loaded.config.model.reserved_output_tokens, 4_096);
    }

    #[test]
    fn rejects_missing_context_window_tokens() {
        let root = unique_dir("missing-context-window");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_MISSING_CONTEXT_WINDOW_KEY"
"#,
        )
        .expect("write config");
        set_env("MORROW_MISSING_CONTEXT_WINDOW_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(err, ConfigError::MissingContextWindowTokens));
    }

    #[test]
    fn rejects_zero_positive_values() {
        let root = unique_dir("context-zero");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_CONTEXT_ZERO_KEY"
context_window_tokens = 0

[context]
summary_target_tokens = 128
"#,
        )
        .expect("write config");
        set_env("MORROW_CONTEXT_ZERO_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(
            err,
            ConfigError::InvalidPositiveValue {
                field: "[model].context_window_tokens"
            }
        ));
    }

    #[test]
    fn rejects_invalid_auto_compact_threshold() {
        let root = unique_dir("context-threshold");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_CONTEXT_THRESHOLD_KEY"
context_window_tokens = 65536

[context]
auto_compact_threshold = 1.5
"#,
        )
        .expect("write config");
        set_env("MORROW_CONTEXT_THRESHOLD_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(err, ConfigError::InvalidAutoCompactThreshold));
    }

    #[test]
    fn rejects_legacy_max_context_chars() {
        let root = unique_dir("legacy-context");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
model = "test-model"
api_key_env = "MORROW_LEGACY_CONTEXT_KEY"
context_window_tokens = 65536

[context]
max_context_chars = 1024
"#,
        )
        .expect("write config");
        set_env("MORROW_LEGACY_CONTEXT_KEY", "secret");

        let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn debug_output_redacts_model_and_mcp_secrets() {
        let root = unique_dir("debug-redaction");
        let config = root.join("morrow.toml");
        fs::write(
            &config,
            r#"
[model]
base_url = "https://example.com/v1?token=model-url-secret"
model = "test-model"
OPENAI_API_KEY = "model-secret"
context_window_tokens = 65536

[mcp_servers.remote]
url = "https://example.com/mcp?token=url-secret"
http_headers = { Authorization = "Bearer mcp-secret" }
"#,
        )
        .expect("write config");

        let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");
        let debug = format!("{loaded:?}");

        assert!(!debug.contains("model-secret"));
        assert!(!debug.contains("model-url-secret"));
        assert!(!debug.contains("mcp-secret"));
        assert!(!debug.contains("url-secret"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("Authorization"));
    }
}
