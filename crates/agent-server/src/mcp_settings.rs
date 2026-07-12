use agent_config::{McpServerConfig, McpTransport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::RwLock;

const MCP_STORE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedMcpTransport {
    Stdio,
    Http,
}

impl From<ManagedMcpTransport> for McpTransport {
    fn from(value: ManagedMcpTransport) -> Self {
        match value {
            ManagedMcpTransport::Stdio => Self::Stdio,
            ManagedMcpTransport::Http => Self::Http,
        }
    }
}

impl From<McpTransport> for ManagedMcpTransport {
    fn from(value: McpTransport) -> Self {
        match value {
            McpTransport::Stdio => Self::Stdio,
            McpTransport::Http => Self::Http,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ManagedMcpServer {
    name: String,
    transport: ManagedMcpTransport,
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_startup_timeout_secs")]
    startup_timeout_sec: u64,
    #[serde(default = "default_tool_timeout_secs")]
    tool_timeout_sec: u64,
}

impl ManagedMcpServer {
    fn runtime_config(&self) -> McpServerConfig {
        McpServerConfig {
            name: self.name.clone(),
            transport: self.transport.into(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            cwd: self.cwd.as_deref().map(PathBuf::from),
            url: self.url.clone(),
            http_headers: self.http_headers.clone(),
            enabled: self.enabled,
            startup_timeout_sec: self.startup_timeout_sec,
            tool_timeout_sec: self.tool_timeout_sec,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedMcpStore {
    schema_version: u32,
    #[serde(default)]
    servers: Vec<ManagedMcpServer>,
}

impl Default for PersistedMcpStore {
    fn default() -> Self {
        Self {
            schema_version: MCP_STORE_SCHEMA_VERSION,
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpSettingsResponse {
    pub servers: Vec<McpServerResponse>,
    pub store_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerResponse {
    pub name: String,
    pub transport: ManagedMcpTransport,
    pub enabled: bool,
    pub read_only: bool,
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub http_header_keys: Vec<String>,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
}

#[derive(Clone, Deserialize)]
pub struct McpServerWriteRequest {
    pub name: String,
    pub transport: ManagedMcpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub http_headers: BTreeMap<String, Option<String>>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_sec: u64,
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_sec: u64,
}

#[derive(Clone, Deserialize)]
pub struct McpServerTestRequest {
    #[serde(default)]
    pub existing_name: Option<String>,
    pub server: McpServerWriteRequest,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportedMcpServer {
    #[serde(rename = "type", default)]
    transport: Option<ManagedMcpTransport>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, alias = "http_headers")]
    headers: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_startup_timeout_secs")]
    startup_timeout_sec: u64,
    #[serde(default = "default_tool_timeout_secs")]
    tool_timeout_sec: u64,
}

#[derive(Debug, Error)]
pub enum McpRegistryError {
    #[error("failed to read MCP settings {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse MCP settings {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported MCP settings schema version {version}; expected {expected}")]
    UnsupportedSchema { version: u32, expected: u32 },
    #[error("failed to create MCP settings directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize MCP settings: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write MCP settings {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace MCP settings {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid MCP settings: {0}")]
    Validation(String),
    #[error("MCP settings conflict: {0}")]
    Conflict(String),
    #[error("MCP server {0:?} was not found")]
    NotFound(String),
}

pub struct McpRegistry {
    path: PathBuf,
    fallback: Vec<McpServerConfig>,
    state: RwLock<PersistedMcpStore>,
}

impl McpRegistry {
    pub fn load(path: PathBuf, fallback: Vec<McpServerConfig>) -> Result<Self, McpRegistryError> {
        let store = load_store(&path)?;
        validate_store(&store, &fallback)?;
        Ok(Self {
            path,
            fallback,
            state: RwLock::new(store),
        })
    }

    pub async fn settings(&self) -> McpSettingsResponse {
        let state = self.state.read().await;
        let mut servers = self
            .fallback
            .iter()
            .map(fallback_response)
            .collect::<Vec<_>>();
        servers.extend(state.servers.iter().map(managed_response));
        McpSettingsResponse {
            servers,
            store_path: self.path.display().to_string(),
        }
    }

    pub async fn effective_servers(&self) -> Vec<McpServerConfig> {
        let state = self.state.read().await;
        let mut servers = self.fallback.clone();
        servers.extend(state.servers.iter().map(ManagedMcpServer::runtime_config));
        servers
    }

    pub async fn create(
        &self,
        request: McpServerWriteRequest,
    ) -> Result<McpServerResponse, McpRegistryError> {
        let mut state = self.state.write().await;
        ensure_name_available(&request.name, None, &state.servers, &self.fallback)?;
        let server = managed_from_request(request, None)?;
        let mut next = state.clone();
        next.servers.push(server.clone());
        next.servers
            .sort_by(|left, right| left.name.cmp(&right.name));
        commit_store(&self.path, &mut state, next)?;
        Ok(managed_response(&server))
    }

    pub async fn update(
        &self,
        current_name: &str,
        request: McpServerWriteRequest,
    ) -> Result<McpServerResponse, McpRegistryError> {
        let mut state = self.state.write().await;
        let index = state
            .servers
            .iter()
            .position(|server| server.name == current_name)
            .ok_or_else(|| McpRegistryError::NotFound(current_name.to_string()))?;
        ensure_name_available(
            &request.name,
            Some(current_name),
            &state.servers,
            &self.fallback,
        )?;
        let server = managed_from_request(request, Some(&state.servers[index]))?;
        let mut next = state.clone();
        next.servers[index] = server.clone();
        next.servers
            .sort_by(|left, right| left.name.cmp(&right.name));
        commit_store(&self.path, &mut state, next)?;
        Ok(managed_response(&server))
    }

    pub async fn delete(&self, name: &str) -> Result<(), McpRegistryError> {
        let mut state = self.state.write().await;
        let mut next = state.clone();
        let previous_len = next.servers.len();
        next.servers.retain(|server| server.name != name);
        if next.servers.len() == previous_len {
            return Err(McpRegistryError::NotFound(name.to_string()));
        }
        commit_store(&self.path, &mut state, next)
    }

    pub async fn import(&self, value: Value) -> Result<Vec<McpServerResponse>, McpRegistryError> {
        let requests = import_requests(value)?;
        let mut state = self.state.write().await;
        let mut next = state.clone();
        let mut imported = Vec::with_capacity(requests.len());
        for request in requests {
            ensure_name_available(&request.name, None, &next.servers, &self.fallback)?;
            let server = managed_from_request(request, None)?;
            imported.push(server.clone());
            next.servers.push(server);
        }
        next.servers
            .sort_by(|left, right| left.name.cmp(&right.name));
        commit_store(&self.path, &mut state, next)?;
        Ok(imported.iter().map(managed_response).collect())
    }

    pub async fn config_for_test(
        &self,
        request: McpServerTestRequest,
    ) -> Result<McpServerConfig, McpRegistryError> {
        let state = self.state.read().await;
        let existing = match request.existing_name.as_deref() {
            Some(name) => Some(
                state
                    .servers
                    .iter()
                    .find(|server| server.name == name)
                    .ok_or_else(|| McpRegistryError::NotFound(name.to_string()))?,
            ),
            None => None,
        };
        let mut server = managed_from_request(request.server, existing)?.runtime_config();
        server.enabled = true;
        Ok(server)
    }
}

fn load_store(path: &Path) -> Result<PersistedMcpStore, McpRegistryError> {
    if !path.is_file() {
        return Ok(PersistedMcpStore::default());
    }
    let content = fs::read_to_string(path).map_err(|source| McpRegistryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let store = serde_json::from_str::<PersistedMcpStore>(&content).map_err(|source| {
        McpRegistryError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if store.schema_version != MCP_STORE_SCHEMA_VERSION {
        return Err(McpRegistryError::UnsupportedSchema {
            version: store.schema_version,
            expected: MCP_STORE_SCHEMA_VERSION,
        });
    }
    Ok(store)
}

fn validate_store(
    store: &PersistedMcpStore,
    fallback: &[McpServerConfig],
) -> Result<(), McpRegistryError> {
    let mut names = fallback
        .iter()
        .map(|server| server.name.as_str())
        .collect::<HashSet<_>>();
    for server in &store.servers {
        validate_managed_server(server)?;
        if !names.insert(server.name.as_str()) {
            return Err(McpRegistryError::Conflict(format!(
                "duplicate MCP server name {:?}",
                server.name
            )));
        }
    }
    Ok(())
}

fn save_store(path: &Path, store: &PersistedMcpStore) -> Result<(), McpRegistryError> {
    let parent = path.parent().ok_or_else(|| {
        McpRegistryError::Validation("MCP settings path has no parent".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|source| McpRegistryError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    let content = serde_json::to_vec_pretty(store).map_err(McpRegistryError::Serialize)?;
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("web-mcp.json"),
        std::process::id()
    ));
    fs::write(&temp, content).map_err(|source| McpRegistryError::Write {
        path: temp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|source| {
            McpRegistryError::Write {
                path: temp.clone(),
                source,
            }
        })?;
    }
    fs::rename(&temp, path).map_err(|source| McpRegistryError::Replace {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn commit_store(
    path: &Path,
    state: &mut PersistedMcpStore,
    next: PersistedMcpStore,
) -> Result<(), McpRegistryError> {
    save_store(path, &next)?;
    *state = next;
    Ok(())
}

fn ensure_name_available(
    name: &str,
    current_name: Option<&str>,
    servers: &[ManagedMcpServer],
    fallback: &[McpServerConfig],
) -> Result<(), McpRegistryError> {
    validate_server_name(name)?;
    if fallback.iter().any(|server| server.name == name) {
        return Err(McpRegistryError::Conflict(format!(
            "MCP server {name:?} is provided by morrow.toml and is read-only"
        )));
    }
    if servers
        .iter()
        .any(|server| server.name == name && current_name != Some(server.name.as_str()))
    {
        return Err(McpRegistryError::Conflict(format!(
            "MCP server {name:?} already exists"
        )));
    }
    Ok(())
}

fn validate_server_name(name: &str) -> Result<(), McpRegistryError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(McpRegistryError::Validation(
            "server name must be 1-64 ASCII letters, digits, '-', '_' or '.', starting with a letter or digit"
                .to_string(),
        ));
    }
    Ok(())
}

fn managed_from_request(
    request: McpServerWriteRequest,
    existing: Option<&ManagedMcpServer>,
) -> Result<ManagedMcpServer, McpRegistryError> {
    let name = request.name.trim().to_string();
    validate_server_name(&name)?;
    if request.startup_timeout_sec == 0 || request.tool_timeout_sec == 0 {
        return Err(McpRegistryError::Validation(
            "MCP timeouts must be greater than zero".to_string(),
        ));
    }
    let env = resolve_secret_map(
        request.env,
        existing.map(|server| &server.env),
        "environment variable",
    )?;
    let http_headers = resolve_secret_map(
        request.http_headers,
        existing.map(|server| &server.http_headers),
        "HTTP header",
    )?;
    let mut server = ManagedMcpServer {
        name,
        transport: request.transport,
        command: request.command.unwrap_or_default().trim().to_string(),
        args: request.args,
        env,
        cwd: request
            .cwd
            .map(|cwd| cwd.trim().to_string())
            .filter(|cwd| !cwd.is_empty()),
        url: request
            .url
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty()),
        http_headers,
        enabled: request.enabled,
        startup_timeout_sec: request.startup_timeout_sec,
        tool_timeout_sec: request.tool_timeout_sec,
    };
    match server.transport {
        ManagedMcpTransport::Stdio => {
            if server.command.is_empty() {
                return Err(McpRegistryError::Validation(
                    "stdio MCP server command must not be empty".to_string(),
                ));
            }
            server.url = None;
            server.http_headers.clear();
        }
        ManagedMcpTransport::Http => {
            let url = server.url.as_deref().unwrap_or_default();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(McpRegistryError::Validation(
                    "HTTP MCP server URL must start with http:// or https://".to_string(),
                ));
            }
            server.command.clear();
            server.args.clear();
            server.env.clear();
            server.cwd = None;
        }
    }
    validate_managed_server(&server)?;
    Ok(server)
}

fn resolve_secret_map(
    requested: BTreeMap<String, Option<String>>,
    existing: Option<&BTreeMap<String, String>>,
    label: &str,
) -> Result<BTreeMap<String, String>, McpRegistryError> {
    let mut resolved = BTreeMap::new();
    for (raw_key, value) in requested {
        let key = raw_key.trim().to_string();
        if key.is_empty() {
            return Err(McpRegistryError::Validation(format!(
                "{label} name must not be empty"
            )));
        }
        let value = match value {
            Some(value) => value,
            None => existing
                .and_then(|values| values.get(&key))
                .cloned()
                .ok_or_else(|| {
                    McpRegistryError::Validation(format!(
                        "{label} {key:?} cannot preserve a value that does not exist"
                    ))
                })?,
        };
        resolved.insert(key, value);
    }
    Ok(resolved)
}

fn validate_managed_server(server: &ManagedMcpServer) -> Result<(), McpRegistryError> {
    validate_server_name(&server.name)?;
    if server.startup_timeout_sec == 0 || server.tool_timeout_sec == 0 {
        return Err(McpRegistryError::Validation(format!(
            "MCP server {:?} has an invalid timeout",
            server.name
        )));
    }
    for key in server.env.keys() {
        if key.trim().is_empty() {
            return Err(McpRegistryError::Validation(format!(
                "MCP server {:?} has an empty environment variable name",
                server.name
            )));
        }
    }
    for (key, value) in &server.http_headers {
        if key.trim().is_empty() || key.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(McpRegistryError::Validation(format!(
                "MCP server {:?} has an invalid HTTP header",
                server.name
            )));
        }
    }
    match server.transport {
        ManagedMcpTransport::Stdio if server.command.trim().is_empty() => {
            Err(McpRegistryError::Validation(format!(
                "stdio MCP server {:?} is missing command",
                server.name
            )))
        }
        ManagedMcpTransport::Http
            if !server
                .url
                .as_deref()
                .is_some_and(|url| url.starts_with("http://") || url.starts_with("https://")) =>
        {
            Err(McpRegistryError::Validation(format!(
                "HTTP MCP server {:?} has an invalid URL",
                server.name
            )))
        }
        _ => Ok(()),
    }
}

fn import_requests(value: Value) -> Result<Vec<McpServerWriteRequest>, McpRegistryError> {
    let mut object = value.as_object().cloned().ok_or_else(|| {
        McpRegistryError::Validation("MCP JSON import must be an object".to_string())
    })?;
    if let Some(wrapped) = object.remove("mcpServers") {
        if !object.is_empty() {
            return Err(McpRegistryError::Validation(
                "mcpServers wrapper must not have sibling fields".to_string(),
            ));
        }
        object = wrapped.as_object().cloned().ok_or_else(|| {
            McpRegistryError::Validation("mcpServers must be an object".to_string())
        })?;
    }
    if object.is_empty() {
        return Err(McpRegistryError::Validation(
            "MCP JSON import must contain at least one server".to_string(),
        ));
    }

    object
        .into_iter()
        .map(|(name, config)| {
            let imported =
                serde_json::from_value::<ImportedMcpServer>(config).map_err(|error| {
                    McpRegistryError::Validation(format!(
                        "invalid MCP JSON for server {name:?}: {error}"
                    ))
                })?;
            let transport = imported.transport.unwrap_or_else(|| {
                if imported.url.is_some() {
                    ManagedMcpTransport::Http
                } else {
                    ManagedMcpTransport::Stdio
                }
            });
            Ok(McpServerWriteRequest {
                name,
                transport,
                command: imported.command,
                args: imported.args,
                env: imported
                    .env
                    .into_iter()
                    .map(|(key, value)| (key, Some(value)))
                    .collect(),
                cwd: imported.cwd,
                url: imported.url,
                http_headers: imported
                    .headers
                    .into_iter()
                    .map(|(key, value)| (key, Some(value)))
                    .collect(),
                enabled: imported.enabled,
                startup_timeout_sec: imported.startup_timeout_sec,
                tool_timeout_sec: imported.tool_timeout_sec,
            })
        })
        .collect()
}

fn fallback_response(server: &McpServerConfig) -> McpServerResponse {
    McpServerResponse {
        name: server.name.clone(),
        transport: server.transport.into(),
        enabled: server.enabled,
        read_only: true,
        source: "runtime_config",
        command: (server.transport == McpTransport::Stdio).then(|| server.command.clone()),
        args: Vec::new(),
        env_keys: server.env.keys().cloned().collect(),
        cwd: server.cwd.as_ref().map(|path| path.display().to_string()),
        url: None,
        http_header_keys: server.http_headers.keys().cloned().collect(),
        startup_timeout_sec: server.startup_timeout_sec,
        tool_timeout_sec: server.tool_timeout_sec,
    }
}

fn managed_response(server: &ManagedMcpServer) -> McpServerResponse {
    McpServerResponse {
        name: server.name.clone(),
        transport: server.transport,
        enabled: server.enabled,
        read_only: false,
        source: "web",
        command: (server.transport == ManagedMcpTransport::Stdio).then(|| server.command.clone()),
        args: server.args.clone(),
        env_keys: server.env.keys().cloned().collect(),
        cwd: server.cwd.clone(),
        url: server.url.clone(),
        http_header_keys: server.http_headers.keys().cloned().collect(),
        startup_timeout_sec: server.startup_timeout_sec,
        tool_timeout_sec: server.tool_timeout_sec,
    }
}

const fn default_true() -> bool {
    true
}

const fn default_startup_timeout_secs() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SECS
}

const fn default_tool_timeout_secs() -> u64 {
    DEFAULT_TOOL_TIMEOUT_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("morrow-mcp-settings-{name}-{stamp}"))
            .join("web-mcp.json")
    }

    fn stdio_request(name: &str) -> McpServerWriteRequest {
        McpServerWriteRequest {
            name: name.to_string(),
            transport: ManagedMcpTransport::Stdio,
            command: Some("npx".to_string()),
            args: vec!["server".to_string()],
            env: BTreeMap::from([("TOKEN".to_string(), Some("secret".to_string()))]),
            cwd: Some(".".to_string()),
            url: None,
            http_headers: BTreeMap::new(),
            enabled: true,
            startup_timeout_sec: 10,
            tool_timeout_sec: 60,
        }
    }

    #[tokio::test]
    async fn managed_secrets_are_persisted_but_not_returned() {
        let path = unique_path("secrets");
        let registry = McpRegistry::load(path.clone(), Vec::new()).expect("registry");

        registry
            .create(stdio_request("docs"))
            .await
            .expect("create");
        let settings = registry.settings().await;
        let content = fs::read_to_string(path).expect("stored settings");
        let response = serde_json::to_string(&settings).expect("response");

        assert!(content.contains("secret"));
        assert!(!response.contains("\"secret\""));
        assert_eq!(settings.servers[0].env_keys, ["TOKEN"]);
    }

    #[tokio::test]
    async fn update_preserves_null_secret_values_and_removes_omitted_keys() {
        let path = unique_path("preserve");
        let registry = McpRegistry::load(path, Vec::new()).expect("registry");
        registry
            .create(stdio_request("docs"))
            .await
            .expect("create");
        let mut request = stdio_request("docs");
        request.env = BTreeMap::from([("TOKEN".to_string(), None)]);

        registry.update("docs", request).await.expect("update");
        let configs = registry.effective_servers().await;

        assert_eq!(
            configs[0].env.get("TOKEN").map(String::as_str),
            Some("secret")
        );
    }

    #[tokio::test]
    async fn import_accepts_direct_and_wrapped_objects() {
        let path = unique_path("import");
        let registry = McpRegistry::load(path, Vec::new()).expect("registry");

        registry
            .import(serde_json::json!({
                "one": {"type": "stdio", "command": "one"}
            }))
            .await
            .expect("direct import");
        registry
            .import(serde_json::json!({
                "mcpServers": {
                    "two": {"type": "http", "url": "https://example.com/mcp"}
                }
            }))
            .await
            .expect("wrapped import");

        assert_eq!(registry.settings().await.servers.len(), 2);
    }

    #[tokio::test]
    async fn fallback_names_cannot_be_overridden() {
        let path = unique_path("fallback");
        let fallback = McpServerConfig {
            name: "docs".to_string(),
            transport: McpTransport::Stdio,
            command: "docs".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            http_headers: BTreeMap::new(),
            enabled: true,
            startup_timeout_sec: 10,
            tool_timeout_sec: 60,
        };
        let registry = McpRegistry::load(path, vec![fallback]).expect("registry");

        let error = registry
            .create(stdio_request("docs"))
            .await
            .expect_err("must reject");

        assert!(matches!(error, McpRegistryError::Conflict(_)));
    }

    #[tokio::test]
    async fn failed_batch_import_does_not_partially_mutate_store() {
        let path = unique_path("atomic-import");
        let registry = McpRegistry::load(path, Vec::new()).expect("registry");
        registry
            .create(stdio_request("existing"))
            .await
            .expect("create existing");

        let error = registry
            .import(serde_json::json!({
                "new-server": {"type": "stdio", "command": "new"},
                "existing": {"type": "stdio", "command": "duplicate"}
            }))
            .await
            .expect_err("batch must fail");

        assert!(matches!(error, McpRegistryError::Conflict(_)));
        assert_eq!(registry.settings().await.servers.len(), 1);
    }

    #[test]
    fn unsupported_store_schema_is_rejected() {
        let path = unique_path("schema");
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, r#"{"schema_version":99,"servers":[]}"#).expect("write store");

        let error = match McpRegistry::load(path, Vec::new()) {
            Ok(_) => panic!("must reject schema"),
            Err(error) => error,
        };

        assert!(matches!(error, McpRegistryError::UnsupportedSchema { .. }));
    }
}
