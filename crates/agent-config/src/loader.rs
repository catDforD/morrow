use super::*;

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

pub(crate) fn load_server_config_from_locations(
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
        server,
        tools,
        mcp_servers,
    } = raw;
    let config = parse_server_app_config(agent, context, permissions, server, tools, mcp_servers)?;
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

pub(crate) fn load_config_from_locations(
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

pub(crate) fn parse_model_config(
    model: RawModelConfig,
) -> Result<(ModelConfig, Option<String>), ConfigError> {
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
            max_retries: model.max_retries,
            context_window_tokens,
            reserved_output_tokens,
        },
        inline_api_key,
    ))
}

pub(crate) fn parse_server_app_config(
    agent: Option<RawAgentConfig>,
    context: Option<RawContextConfig>,
    permissions: Option<RawPermissionsConfig>,
    server: Option<RawServerConfig>,
    tools: Option<RawToolsConfig>,
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
    let server = server.unwrap_or_default();
    let tools = tools.unwrap_or_default();

    Ok(ServerAppConfig {
        agent: AgentConfig {
            system_prompt: agent
                .system_prompt
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        },
        context,
        permissions: permissions_profile,
        workspace_write_require_approval: permissions
            .workspace_write_require_approval
            .unwrap_or(false),
        mcp_servers: parse_mcp_servers(mcp_servers)?,
        server: ServerConfig {
            permission_ceiling: server
                .permission_ceiling
                .unwrap_or(PermissionMode::DangerFullAccess),
        },
        tools: ToolsConfig {
            allow: tools.allow.unwrap_or_default(),
            deny: tools.deny.unwrap_or_default(),
        },
    })
}

pub(crate) fn positive_config_value(
    field: &'static str,
    value: usize,
) -> Result<usize, ConfigError> {
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
                    require_approval: raw.require_approval,
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
                    require_approval: raw.require_approval,
                });
            }
        }
    }
    Ok(servers)
}
