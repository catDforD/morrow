use super::*;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAppConfig {
    pub(crate) model: Option<RawModelConfig>,
    pub(crate) agent: Option<RawAgentConfig>,
    pub(crate) context: Option<RawContextConfig>,
    pub(crate) permissions: Option<RawPermissionsConfig>,
    pub(crate) server: Option<RawServerConfig>,
    pub(crate) tools: Option<RawToolsConfig>,
    #[serde(default)]
    pub(crate) mcp_servers: BTreeMap<String, RawMcpServerConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawModelConfig {
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) api_key_env: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    pub(crate) openai_api_key: Option<String>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) max_retries: Option<u32>,
    pub(crate) context_window_tokens: Option<usize>,
    pub(crate) reserved_output_tokens: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAgentConfig {
    pub(crate) system_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawContextConfig {
    pub(crate) auto_compact: Option<bool>,
    pub(crate) auto_compact_threshold: Option<f32>,
    pub(crate) retain_recent_turns: Option<usize>,
    pub(crate) summary_target_tokens: Option<usize>,
    pub(crate) compact_max_retries: Option<usize>,
    pub(crate) max_context_tokens: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPermissionsConfig {
    pub(crate) mode: Option<PermissionMode>,
    pub(crate) shell: Option<ShellPolicy>,
    pub(crate) workspace_write_require_approval: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawServerConfig {
    pub(crate) permission_ceiling: Option<PermissionMode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawToolsConfig {
    pub(crate) allow: Option<Vec<String>>,
    pub(crate) deny: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpServerConfig {
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<String>,
    pub(crate) enabled: Option<bool>,
    pub(crate) startup_timeout_sec: Option<u64>,
    pub(crate) tool_timeout_sec: Option<u64>,
    pub(crate) require_approval: Option<bool>,
    pub(crate) url: Option<String>,
    pub(crate) bearer_token_env_var: Option<String>,
    #[serde(default)]
    pub(crate) http_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) env_http_headers: BTreeMap<String, String>,
    pub(crate) oauth_client_id: Option<String>,
    pub(crate) oauth_resource: Option<String>,
}
