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
max_context_tokens = 150000
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

    let loaded = load_config_from_locations(None, &cwd, Some(&home)).expect("load local config");

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

    let loaded = load_server_config_from_locations(None, &cwd, Some(&home)).expect("server config");

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

    let loaded = load_server_config_from_locations(None, &cwd, Some(&home)).expect("server config");

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
    assert_eq!(loaded.config.model.max_retries, None);
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
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        }
    );
    assert_eq!(
        loaded.config.permissions,
        PermissionProfile::for_mode(PermissionMode::ReadOnly)
    );
    assert!(loaded.config.mcp_servers.is_empty());
}

#[test]
fn loads_model_max_retries() {
    let root = unique_dir("model-max-retries");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[model]
model = "test-model"
api_key_env = "MORROW_MAX_RETRIES_KEY"
context_window_tokens = 65536
max_retries = 0
"#,
    )
    .expect("write config");
    set_env("MORROW_MAX_RETRIES_KEY", "secret");

    let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

    assert_eq!(loaded.config.model.max_retries, Some(0));
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
require_approval = false
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
    assert_eq!(server.require_approval, Some(false));
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
    assert_eq!(server.require_approval, None);
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
    assert!(!loaded.config.workspace_write_require_approval);
}

#[test]
fn loads_workspace_write_require_approval() {
    let root = unique_dir("workspace-write-require-approval");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[permissions]
mode = "workspace_write"
workspace_write_require_approval = true
"#,
    )
    .expect("write config");

    let loaded =
        load_server_config_from_locations(Some(&config), &root, None).expect("load server config");

    assert!(loaded.config.workspace_write_require_approval);
}

#[test]
fn loads_server_permission_ceiling() {
    let root = unique_dir("server-ceiling");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[server]
permission_ceiling = "workspace_write"
"#,
    )
    .expect("write config");

    let loaded =
        load_server_config_from_locations(Some(&config), &root, None).expect("load server config");

    assert_eq!(
        loaded.config.server.permission_ceiling,
        PermissionMode::WorkspaceWrite
    );
}

#[test]
fn server_permission_ceiling_defaults_to_no_cap() {
    let root = unique_dir("server-ceiling-default");
    let config = root.join("morrow.toml");
    fs::write(&config, "").expect("write config");

    let loaded =
        load_server_config_from_locations(Some(&config), &root, None).expect("load server config");

    assert_eq!(
        loaded.config.server,
        ServerConfig {
            permission_ceiling: PermissionMode::DangerFullAccess,
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
            max_context_tokens: Some(150_000),
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
fn rejects_zero_max_context_tokens() {
    let root = unique_dir("context-max-tokens");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[model]
model = "test-model"
api_key_env = "MORROW_CONTEXT_MAX_TOKENS_KEY"
context_window_tokens = 65536

[context]
max_context_tokens = 0
"#,
    )
    .expect("write config");
    set_env("MORROW_CONTEXT_MAX_TOKENS_KEY", "secret");

    let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

    assert!(matches!(
        err,
        ConfigError::InvalidPositiveValue {
            field: "[context].max_context_tokens"
        }
    ));
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

#[test]
fn loads_tools_allow_deny_config() {
    let root = unique_dir("tools-allow-deny");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[model]
model = "test-model"
api_key_env = "MORROW_TOOLS_CONFIG_KEY"
context_window_tokens = 65536

[tools]
allow = ["read_file", "mcp__docs__*"]
deny = ["shell_command"]
"#,
    )
    .expect("write config");
    set_env("MORROW_TOOLS_CONFIG_KEY", "secret");

    let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

    assert_eq!(
        loaded.config.tools,
        ToolsConfig {
            allow: vec!["read_file".to_string(), "mcp__docs__*".to_string()],
            deny: vec!["shell_command".to_string()],
        }
    );
}

#[test]
fn tools_config_defaults_to_allowing_everything() {
    let root = unique_dir("tools-default");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[model]
model = "test-model"
api_key_env = "MORROW_TOOLS_DEFAULT_KEY"
context_window_tokens = 65536
"#,
    )
    .expect("write config");
    set_env("MORROW_TOOLS_DEFAULT_KEY", "secret");

    let loaded = load_config_from_locations(Some(&config), &root, None).expect("load config");

    assert_eq!(loaded.config.tools, ToolsConfig::default());
    assert!(loaded.config.tools.allows("shell_command"));
    assert!(loaded.config.tools.allows("mcp__docs__read"));
}

#[test]
fn tools_config_rejects_unknown_fields() {
    let root = unique_dir("tools-unknown-field");
    let config = root.join("morrow.toml");
    fs::write(
        &config,
        r#"
[model]
model = "test-model"
api_key_env = "MORROW_TOOLS_UNKNOWN_KEY"
context_window_tokens = 65536

[tools]
allowed = ["read_file"]
"#,
    )
    .expect("write config");
    set_env("MORROW_TOOLS_UNKNOWN_KEY", "secret");

    let err = load_config_from_locations(Some(&config), &root, None).expect_err("must fail");

    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn tools_config_matching_matrix() {
    let config = ToolsConfig::default();
    assert!(config.allows("read_file"));
    assert!(config.allows("mcp__docs__read"));

    // 内置工具名精确匹配。
    let config = ToolsConfig {
        allow: vec!["read_file".to_string()],
        deny: Vec::new(),
    };
    assert!(config.allows("read_file"));
    assert!(!config.allows("read_file_extra"));
    assert!(!config.allows("write_file"));

    // mcp__server 匹配整个 server，要求 "__" 边界。
    let config = ToolsConfig {
        allow: vec!["mcp__docs".to_string()],
        deny: Vec::new(),
    };
    assert!(config.allows("mcp__docs"));
    assert!(config.allows("mcp__docs__read"));
    assert!(!config.allows("mcp__docs2__read"));
    assert!(!config.allows("mcp__fs__read"));

    // 前缀通配。
    let config = ToolsConfig {
        allow: vec!["mcp__docs__*".to_string()],
        deny: Vec::new(),
    };
    assert!(config.allows("mcp__docs__read"));
    assert!(!config.allows("mcp__fs__read"));
    assert!(!config.allows("read_file"));

    // deny 优先于 allow。
    let config = ToolsConfig {
        allow: vec!["mcp__docs".to_string()],
        deny: vec!["mcp__docs__write".to_string()],
    };
    assert!(config.allows("mcp__docs__read"));
    assert!(!config.allows("mcp__docs__write"));

    // 空 allow + deny 内置工具。
    let config = ToolsConfig {
        allow: Vec::new(),
        deny: vec!["shell_command".to_string()],
    };
    assert!(!config.allows("shell_command"));
    assert!(config.allows("read_file"));

    // 空条目与空白条目不匹配任何工具。
    let config = ToolsConfig {
        allow: vec![String::new()],
        deny: vec!["  ".to_string()],
    };
    assert!(!config.allows("read_file"));
}
