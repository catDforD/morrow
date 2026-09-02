use super::*;

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(crate) const DEFAULT_RESERVED_OUTPUT_TOKENS: usize = 8_192;
pub(crate) const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
pub(crate) const DEFAULT_AUTO_COMPACT: bool = true;
pub(crate) const DEFAULT_AUTO_COMPACT_THRESHOLD: f32 = 0.835;
pub(crate) const DEFAULT_RETAIN_RECENT_TURNS: usize = 6;
pub(crate) const DEFAULT_SUMMARY_TARGET_TOKENS: usize = 12_000;
pub(crate) const DEFAULT_COMPACT_MAX_RETRIES: usize = 2;
pub(crate) const DEFAULT_MAX_CONTEXT_TOKENS: usize = 300_000;
pub(crate) const DEFAULT_MCP_STARTUP_TIMEOUT_SECS: u64 = 10;
pub(crate) const DEFAULT_MCP_TOOL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub permissions: PermissionProfile,
    /// workspace_write 模式下 workspace 内文件变更是否仍需逐次审批；
    /// 默认 false（自动放行），设为 true 恢复旧的逐次确认行为。
    pub workspace_write_require_approval: bool,
    pub mcp_servers: Vec<McpServerConfig>,
    pub tools: ToolsConfig,
}

/// 工具级 allow/deny：`deny` 优先于 `allow`，空 `allow` 表示全部允许。
/// 条目支持内置工具名精确匹配、`mcp__server`（整个 server）与 `mcp__server__*` 前缀通配。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolsConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl ToolsConfig {
    pub fn allows(&self, tool_name: &str) -> bool {
        if self
            .deny
            .iter()
            .any(|pattern| tool_name_matches_pattern(pattern, tool_name))
        {
            return false;
        }
        self.allow.is_empty()
            || self
                .allow
                .iter()
                .any(|pattern| tool_name_matches_pattern(pattern, tool_name))
    }
}

fn tool_name_matches_pattern(pattern: &str, tool_name: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    tool_name == pattern
        || tool_name
            .strip_prefix(pattern)
            .is_some_and(|rest| rest.starts_with("__"))
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_secs: u64,
    /// 单次模型请求的最大尝试次数（含首次）；`None` 使用默认值 3，`Some(0)` 禁用重试。
    pub max_retries: Option<u32>,
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
            .field("max_retries", &self.max_retries)
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
    /// 上下文水位的绝对上限：压缩触发点与 turn 内护栏都不会超过它。
    /// `None` 只保留模型窗口百分比阈值（TOML 配置始终解析为 `Some(_)`）。
    pub max_context_tokens: Option<usize>,
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
    /// 非只读工具调用是否需要审批；`None` 等价于 `Some(true)`。
    pub require_approval: Option<bool>,
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
            .field("require_approval", &self.require_approval)
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
    /// 见 AppConfig::workspace_write_require_approval。
    pub workspace_write_require_approval: bool,
    pub mcp_servers: Vec<McpServerConfig>,
    pub server: ServerConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// Cap on the permission mode web clients may request per turn.
    pub permission_ceiling: PermissionMode,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            permission_ceiling: PermissionMode::DangerFullAccess,
        }
    }
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

impl TryFrom<RawAppConfig> for AppConfig {
    type Error = ConfigError;

    fn try_from(value: RawAppConfig) -> Result<Self, Self::Error> {
        let RawAppConfig {
            model,
            agent,
            context,
            permissions,
            server,
            tools,
            mcp_servers,
        } = value;
        let (model, _) = parse_model_config(model.unwrap_or_default())?;
        let server =
            parse_server_app_config(agent, context, permissions, server, tools, mcp_servers)?;

        Ok(Self {
            model,
            agent: server.agent,
            context: server.context,
            permissions: server.permissions,
            workspace_write_require_approval: server.workspace_write_require_approval,
            mcp_servers: server.mcp_servers,
            tools: server.tools,
        })
    }
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
        let max_context_tokens = match value.max_context_tokens {
            Some(value) => Some(positive_config_value(
                "[context].max_context_tokens",
                value,
            )?),
            None => Some(DEFAULT_MAX_CONTEXT_TOKENS),
        };

        Ok(Self {
            auto_compact: value.auto_compact.unwrap_or(DEFAULT_AUTO_COMPACT),
            auto_compact_threshold,
            retain_recent_turns,
            summary_target_tokens,
            compact_max_retries,
            max_context_tokens,
        })
    }
}
