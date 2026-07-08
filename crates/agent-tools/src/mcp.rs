use crate::{Tool, ToolExecution, ToolResult};
use agent_config::{McpServerConfig, McpTransport};
use agent_protocol::{ToolCall, ToolDefinition, ToolExecutionSummary};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const STDERR_TAIL_BYTES: usize = 8192;

#[derive(Default)]
pub struct McpToolCache {
    entries: Mutex<HashMap<McpServerKey, Arc<CachedMcpServer>>>,
}

impl McpToolCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get_or_start(
        &self,
        config: &McpServerConfig,
        cwd: PathBuf,
    ) -> Result<Arc<CachedMcpServer>, String> {
        let key = McpServerKey::from_config(config, &cwd);
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "MCP cache lock poisoned".to_string())?;

        if let Some(entry) = entries.get(&key) {
            if entry.runtime.is_healthy() {
                return Ok(entry.clone());
            }
            entries.remove(&key);
        }

        let entry = start_mcp_server(config, cwd)?;
        entries.insert(key, entry.clone());
        Ok(entry)
    }
}

impl std::fmt::Debug for McpToolCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entry_count = self
            .entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0);
        formatter
            .debug_struct("McpToolCache")
            .field("entry_count", &entry_count)
            .finish()
    }
}

pub struct McpDiscovery {
    pub tools: Vec<Arc<dyn Tool>>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct McpServerKey {
    name: String,
    transport: McpTransport,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    url: Option<String>,
    http_headers: BTreeMap<String, String>,
    startup_timeout_sec: u64,
    tool_timeout_sec: u64,
}

impl McpServerKey {
    fn from_config(config: &McpServerConfig, cwd: &Path) -> Self {
        Self {
            name: config.name.clone(),
            transport: config.transport,
            command: config.command.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            cwd: cwd.to_path_buf(),
            url: config.url.clone(),
            http_headers: config.http_headers.clone(),
            startup_timeout_sec: config.startup_timeout_sec,
            tool_timeout_sec: config.tool_timeout_sec,
        }
    }
}

struct CachedMcpServer {
    runtime: Arc<McpServerRuntime>,
    listed_tools: Vec<ListedTool>,
}

pub fn discover_tools(
    workspace_root: &Path,
    servers: &[McpServerConfig],
    cache: &McpToolCache,
) -> McpDiscovery {
    let mut tools = Vec::new();
    let mut diagnostics = Vec::new();
    let mut emitted_names = BTreeSet::new();

    for server in servers.iter().filter(|server| server.enabled) {
        let cwd = resolve_cwd(workspace_root, server.cwd.as_deref());
        let entry = match cache.get_or_start(server, cwd) {
            Ok(entry) => entry,
            Err(message) => {
                diagnostics.push(format!("mcp server {}: {message}", server.name));
                continue;
            }
        };

        if let Some(tool) =
            build_tool_provider(&server.name, entry, &mut emitted_names, &mut diagnostics)
        {
            tools.push(tool as Arc<dyn Tool>);
        }
    }

    McpDiscovery { tools, diagnostics }
}

fn start_mcp_server(
    config: &McpServerConfig,
    cwd: PathBuf,
) -> Result<Arc<CachedMcpServer>, String> {
    let startup_timeout = Duration::from_secs(config.startup_timeout_sec);
    let tool_timeout = Duration::from_secs(config.tool_timeout_sec);
    let mut transport = McpTransportClient::start(config, cwd, startup_timeout)?;
    let listed_tools = initialize_and_list_tools(&mut transport, startup_timeout)?;
    let runtime = Arc::new(McpServerRuntime {
        name: config.name.clone(),
        transport: Mutex::new(transport),
        tool_timeout,
    });

    Ok(Arc::new(CachedMcpServer {
        runtime,
        listed_tools,
    }))
}

fn resolve_cwd(workspace_root: &Path, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => workspace_root.join(path),
        None => workspace_root.to_path_buf(),
    }
}

fn build_tool_provider(
    server_name: &str,
    entry: Arc<CachedMcpServer>,
    emitted_names: &mut BTreeSet<String>,
    diagnostics: &mut Vec<String>,
) -> Option<Arc<McpToolProvider>> {
    let (definitions, lookup) =
        build_tool_definitions(server_name, &entry.listed_tools, emitted_names, diagnostics);

    (!definitions.is_empty()).then(|| {
        Arc::new(McpToolProvider {
            runtime: entry.runtime.clone(),
            definitions,
            lookup,
        })
    })
}

fn build_tool_definitions(
    server_name: &str,
    tools: &[ListedTool],
    emitted_names: &mut BTreeSet<String>,
    diagnostics: &mut Vec<String>,
) -> (Vec<ToolDefinition>, HashMap<String, String>) {
    let mut server_names = BTreeSet::new();
    let mut definitions = Vec::with_capacity(tools.len());
    let mut lookup = HashMap::with_capacity(tools.len());

    for tool in tools {
        let Some(normalized) = build_tool_name(server_name, &tool.name) else {
            diagnostics.push(format!(
                "mcp server {server_name}: skipped tool {:?}: normalized tool name is empty",
                tool.name
            ));
            continue;
        };

        if !server_names.insert(normalized.clone()) {
            diagnostics.push(format!(
                "mcp server {server_name}: skipped duplicate tool after normalization: {normalized}"
            ));
            continue;
        }

        if !emitted_names.insert(normalized.clone()) {
            diagnostics.push(format!(
                "mcp server {server_name}: skipped duplicate MCP tool name after normalization: {normalized}"
            ));
            continue;
        }

        let description = match tool
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(description) => format!("MCP tool from server '{server_name}': {description}"),
            None => format!("MCP tool from server '{server_name}'."),
        };
        let parameters = tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        definitions.push(ToolDefinition::function(
            normalized.clone(),
            description,
            parameters,
        ));
        lookup.insert(normalized, tool.name.clone());
    }

    (definitions, lookup)
}

struct McpToolProvider {
    runtime: Arc<McpServerRuntime>,
    definitions: Vec<ToolDefinition>,
    lookup: HashMap<String, String>,
}

impl Tool for McpToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    fn execute(
        &self,
        call: &ToolCall,
        _approval: Option<(
            &agent_protocol::ApprovalDecision,
            &agent_protocol::ApprovalRequest,
        )>,
    ) -> ToolExecution {
        let Some(original_name) = self.lookup.get(&call.function.name) else {
            return ToolExecution::error(format!("unknown MCP tool {:?}", call.function.name));
        };
        let arguments = match serde_json::from_str::<Value>(&call.function.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolExecution::error(format!(
                    "invalid arguments for tool {}: {error}",
                    call.function.name
                ));
            }
        };

        let result = self.runtime.call_tool(original_name, arguments);
        ToolExecution::Completed(match result {
            Ok(result) => mcp_call_result(&self.runtime.name, original_name, result),
            Err(error) => tool_error_json(error),
        })
    }
}

struct McpServerRuntime {
    name: String,
    transport: Mutex<McpTransportClient>,
    tool_timeout: Duration,
}

impl McpServerRuntime {
    fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<CallToolResult, String> {
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| "MCP transport lock poisoned".to_string())?;
        call_tool(&mut *transport, tool_name, arguments, self.tool_timeout)
    }

    fn is_healthy(&self) -> bool {
        self.transport
            .lock()
            .map(|mut transport| transport.is_healthy())
            .unwrap_or(false)
    }
}

trait JsonRpcTransport {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;

    fn notify(&mut self, method: &str, params: Value, timeout: Duration) -> Result<(), String>;
}

enum McpTransportClient {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl McpTransportClient {
    fn start(
        config: &McpServerConfig,
        cwd: PathBuf,
        startup_timeout: Duration,
    ) -> Result<Self, String> {
        match config.transport {
            McpTransport::Stdio => {
                StdioTransport::start(config, cwd, startup_timeout).map(Self::Stdio)
            }
            McpTransport::Http => HttpTransport::start(config).map(Self::Http),
        }
    }

    fn is_healthy(&mut self) -> bool {
        match self {
            Self::Stdio(transport) => transport.is_healthy(),
            Self::Http(_) => true,
        }
    }
}

impl JsonRpcTransport for McpTransportClient {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        match self {
            Self::Stdio(transport) => transport.request(method, params, timeout),
            Self::Http(transport) => transport.request(method, params, timeout),
        }
    }

    fn notify(&mut self, method: &str, params: Value, timeout: Duration) -> Result<(), String> {
        match self {
            Self::Stdio(transport) => transport.notify(method, params, timeout),
            Self::Http(transport) => transport.notify(method, params, timeout),
        }
    }
}

struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Value, String>>,
    stderr_tail: Arc<Mutex<TailBuffer>>,
    next_id: u64,
    failed: bool,
}

impl StdioTransport {
    fn start(
        config: &McpServerConfig,
        cwd: PathBuf,
        startup_timeout: Duration,
    ) -> Result<Self, String> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(cwd)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start MCP stdio server: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to capture MCP server stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture MCP server stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture MCP server stderr".to_string())?;
        let stderr_tail = Arc::new(Mutex::new(TailBuffer::new(STDERR_TAIL_BYTES)));

        let (tx, responses) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let message = match line {
                    Ok(line) if line.trim().is_empty() => continue,
                    Ok(line) => serde_json::from_str::<Value>(&line).map_err(|error| {
                        format!("failed to parse MCP JSON-RPC message: {error}; line: {line}")
                    }),
                    Err(error) => Err(format!("failed to read MCP server stdout: {error}")),
                };
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        spawn_stderr_tail(stderr, stderr_tail.clone());

        let mut transport = Self {
            child,
            stdin,
            responses,
            stderr_tail,
            next_id: 1,
            failed: false,
        };
        transport.ensure_started(startup_timeout)?;
        Ok(transport)
    }

    fn ensure_started(&mut self, timeout: Duration) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.fail(format!("MCP stdio server exited during startup: {status}"))
            }
            Ok(None) => {
                if timeout.is_zero() {
                    self.fail("MCP startup timeout must be greater than zero".to_string())
                } else {
                    Ok(())
                }
            }
            Err(error) => self.fail(format!("failed to inspect MCP server status: {error}")),
        }
    }

    fn is_healthy(&mut self) -> bool {
        if self.failed {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                self.failed = true;
                false
            }
        }
    }

    fn write_message(&mut self, message: Value) -> Result<(), String> {
        if let Err(error) = serde_json::to_writer(&mut self.stdin, &message) {
            return self.fail(format!("failed to encode MCP JSON-RPC message: {error}"));
        }
        if let Err(error) = self.stdin.write_all(b"\n") {
            return self.fail(format!("failed to write MCP JSON-RPC message: {error}"));
        }
        if let Err(error) = self.stdin.flush() {
            return self.fail(format!("failed to flush MCP JSON-RPC message: {error}"));
        }
        Ok(())
    }

    fn fail<T>(&mut self, message: String) -> Result<T, String> {
        self.failed = true;
        Err(self.with_stderr_tail(message))
    }

    fn with_stderr_tail(&self, message: String) -> String {
        let tail = self.stderr_tail();
        if tail.is_empty() {
            message
        } else {
            format!("{message}; stderr tail: {tail}")
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| tail.as_string())
            .unwrap_or_else(|_| "stderr tail unavailable: lock poisoned".to_string())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl JsonRpcTransport for StdioTransport {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        loop {
            let message = match self.responses.recv_timeout(timeout) {
                Ok(Ok(message)) => message,
                Ok(Err(error)) => return self.fail(error),
                Err(RecvTimeoutError::Timeout) => {
                    return self.fail(format!("MCP request {method} timed out"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return self.fail(format!(
                        "MCP request {method} failed: MCP server stdout closed"
                    ));
                }
            };

            let Some(response_id) = message.get("id") else {
                continue;
            };
            if response_id != &json!(id) {
                return self.fail(format!(
                    "MCP response id mismatch for {method}: expected {id}, got {response_id}"
                ));
            }
            if let Some(error) = message.get("error") {
                return Err(format!("MCP request {method} failed: {error}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value, _timeout: Duration) -> Result<(), String> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }
}

struct HttpTransport {
    client: reqwest::blocking::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    protocol_version: String,
    next_id: u64,
}

impl HttpTransport {
    fn start(config: &McpServerConfig) -> Result<Self, String> {
        let url = config
            .url
            .clone()
            .ok_or_else(|| "HTTP MCP server is missing url".to_string())?;
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|error| format!("failed to build MCP HTTP client: {error}"))?;
        Ok(Self {
            client,
            url,
            headers: config.http_headers.clone(),
            session_id: None,
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            next_id: 1,
        })
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request_message(
        &mut self,
        message: Value,
        id: u64,
        timeout: Duration,
    ) -> Result<Value, String> {
        let had_session = self.session_id.is_some();
        match self.post_jsonrpc(
            &message,
            HttpResponseKind::Request { expected_id: id },
            timeout,
        ) {
            Ok(result) => Ok(result),
            Err(HttpPostError::SessionExpired) if had_session => {
                self.reinitialize(timeout)?;
                self.post_jsonrpc(
                    &message,
                    HttpResponseKind::Request { expected_id: id },
                    timeout,
                )
                .map_err(HttpPostError::into_message)
            }
            Err(error) => Err(error.into_message()),
        }
    }

    fn reinitialize(&mut self, timeout: Duration) -> Result<(), String> {
        self.session_id = None;
        self.protocol_version = MCP_PROTOCOL_VERSION.to_string();
        let id = self.next_request_id();
        let result = self.request_message(initialize_request(id), id, timeout)?;
        self.apply_initialize_result(&result);
        self.notify("notifications/initialized", json!({}), timeout)
    }

    fn apply_initialize_result(&mut self, result: &Value) {
        if let Some(protocol_version) = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|version| !version.trim().is_empty())
        {
            self.protocol_version = protocol_version.to_string();
        }
    }

    fn post_jsonrpc(
        &mut self,
        message: &Value,
        kind: HttpResponseKind,
        timeout: Duration,
    ) -> Result<Value, HttpPostError> {
        let had_session = self.session_id.is_some();
        let mut request = self.client.post(&self.url).timeout(timeout).json(message);

        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request = request
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", self.protocol_version.as_str());
        if let Some(session_id) = self.session_id.as_deref() {
            request = request.header("Mcp-Session-Id", session_id);
        }

        let response = request.send().map_err(|error| {
            if error.is_timeout() {
                HttpPostError::Failed(format!("MCP HTTP request to {} timed out", self.url))
            } else {
                HttpPostError::Failed(format!("failed to send MCP HTTP request: {error}"))
            }
        })?;
        let status = response.status();
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get("Content-Type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response.text().map_err(|error| {
            HttpPostError::Failed(format!("failed to read MCP HTTP body: {error}"))
        })?;

        if status == StatusCode::NOT_FOUND && had_session {
            return Err(HttpPostError::SessionExpired);
        }
        if let Some(session_id) = session_id {
            self.session_id = Some(session_id);
        }

        parse_http_response(status, content_type.as_deref(), &body, kind)
            .map_err(HttpPostError::Failed)
    }
}

impl JsonRpcTransport for HttpTransport {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_request_id();
        if method == "initialize" {
            self.session_id = None;
            self.protocol_version = MCP_PROTOCOL_VERSION.to_string();
        }
        let result = self.request_message(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
            id,
            timeout,
        )?;
        if method == "initialize" {
            self.apply_initialize_result(&result);
        }
        Ok(result)
    }

    fn notify(&mut self, method: &str, params: Value, timeout: Duration) -> Result<(), String> {
        self.post_jsonrpc(
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
            HttpResponseKind::Notification,
            timeout,
        )
        .map(|_| ())
        .map_err(HttpPostError::into_message)
    }
}

#[derive(Clone, Copy)]
enum HttpResponseKind {
    Request { expected_id: u64 },
    Notification,
}

enum HttpPostError {
    SessionExpired,
    Failed(String),
}

impl HttpPostError {
    fn into_message(self) -> String {
        match self {
            Self::SessionExpired => "MCP HTTP session expired".to_string(),
            Self::Failed(message) => message,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HttpJsonRpcResponse {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "morrow",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    })
}

fn parse_http_response(
    status: StatusCode,
    content_type: Option<&str>,
    body: &str,
    kind: HttpResponseKind,
) -> Result<Value, String> {
    if matches!(kind, HttpResponseKind::Notification) && status == StatusCode::ACCEPTED {
        return Ok(Value::Null);
    }
    if !status.is_success() {
        return Err(format!("MCP HTTP status {status}: {body}"));
    }
    if body.trim().is_empty() {
        return match kind {
            HttpResponseKind::Notification => Ok(Value::Null),
            HttpResponseKind::Request { .. } => Err("MCP HTTP response body was empty".to_string()),
        };
    }

    match content_type.map(str::to_ascii_lowercase) {
        Some(content_type) if content_type.starts_with("text/event-stream") => {
            parse_sse_http_response(body, kind)
        }
        _ => parse_json_http_response(body, kind),
    }
}

fn parse_json_http_response(body: &str, kind: HttpResponseKind) -> Result<Value, String> {
    let response = serde_json::from_str::<HttpJsonRpcResponse>(body).map_err(|error| {
        format!("failed to parse MCP HTTP JSON-RPC response: {error}; body: {body}")
    })?;
    response_result(response, kind)
}

fn parse_sse_http_response(body: &str, kind: HttpResponseKind) -> Result<Value, String> {
    let expected_id = match kind {
        HttpResponseKind::Request { expected_id } => expected_id,
        HttpResponseKind::Notification => return Ok(Value::Null),
    };
    let mut data_lines = Vec::new();
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(result) = parse_sse_event(&data_lines, expected_id)? {
                return Ok(result);
            }
            data_lines.clear();
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }
    if let Some(result) = parse_sse_event(&data_lines, expected_id)? {
        return Ok(result);
    }
    Err("MCP HTTP SSE response did not contain a matching JSON-RPC response".to_string())
}

fn parse_sse_event(data_lines: &[String], expected_id: u64) -> Result<Option<Value>, String> {
    if data_lines.is_empty() {
        return Ok(None);
    }
    let data = data_lines.join("\n");
    let response = serde_json::from_str::<HttpJsonRpcResponse>(&data)
        .map_err(|error| format!("failed to parse MCP HTTP SSE event: {error}; data: {data}"))?;
    if response.id != Some(json!(expected_id)) {
        return Ok(None);
    }
    response_result(response, HttpResponseKind::Request { expected_id }).map(Some)
}

fn response_result(response: HttpJsonRpcResponse, kind: HttpResponseKind) -> Result<Value, String> {
    if let Some(error) = response.error {
        return Err(format!("MCP HTTP JSON-RPC error: {error}"));
    }
    match kind {
        HttpResponseKind::Request { expected_id } => {
            let expected = json!(expected_id);
            if response.id.as_ref() != Some(&expected) {
                return Err(format!(
                    "MCP HTTP response id mismatch: expected {expected}, got {:?}",
                    response.id
                ));
            }
        }
        HttpResponseKind::Notification => {}
    }
    Ok(response.result.unwrap_or(Value::Null))
}

fn spawn_stderr_tail(stderr: ChildStderr, tail: Arc<Mutex<TailBuffer>>) {
    thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes) => {
                    if let Ok(mut tail) = tail.lock() {
                        tail.push(&buffer[..bytes]);
                    }
                }
                Err(error) => {
                    if let Ok(mut tail) = tail.lock() {
                        tail.push(format!("\n[stderr read error: {error}]").as_bytes());
                    }
                    break;
                }
            }
        }
    });
}

struct TailBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl TailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > self.limit {
            let overflow = self.bytes.len() - self.limit;
            self.bytes.drain(..overflow);
        }
    }

    fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).trim().to_string()
    }
}

fn initialize_and_list_tools<T: JsonRpcTransport>(
    transport: &mut T,
    timeout: Duration,
) -> Result<Vec<ListedTool>, String> {
    transport.request(
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "morrow",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
        timeout,
    )?;
    transport.notify("notifications/initialized", json!({}), timeout)?;
    list_tools(transport, timeout)
}

fn list_tools<T: JsonRpcTransport>(
    transport: &mut T,
    timeout: Duration,
) -> Result<Vec<ListedTool>, String> {
    let mut cursor = None::<String>;
    let mut tools = Vec::new();
    loop {
        let params = match cursor.as_deref() {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let result = transport.request("tools/list", params, timeout)?;
        let list = serde_json::from_value::<ListToolsResult>(result)
            .map_err(|error| format!("invalid tools/list response: {error}"))?;
        tools.extend(list.tools);
        cursor = list.next_cursor.filter(|cursor| !cursor.is_empty());
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

fn call_tool<T: JsonRpcTransport>(
    transport: &mut T,
    tool_name: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<CallToolResult, String> {
    let result = transport.request(
        "tools/call",
        json!({
            "name": tool_name,
            "arguments": arguments,
        }),
        timeout,
    )?;
    serde_json::from_value::<CallToolResult>(result)
        .map_err(|error| format!("invalid tools/call response: {error}"))
}

pub fn build_tool_name(server: &str, tool: &str) -> Option<String> {
    let server = normalize_component(server);
    let tool = normalize_component(tool);
    (!server.is_empty() && !tool.is_empty()).then(|| format!("mcp__{server}__{tool}"))
}

fn normalize_component(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut last_sep = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            output.push('_');
            last_sep = true;
        }
    }

    output.trim_matches('_').to_string()
}

#[derive(Debug, Deserialize)]
struct ListToolsResult {
    #[serde(default)]
    tools: Vec<ListedTool>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ListedTool {
    name: String,
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallToolResult {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    structured_content: Option<Value>,
    #[serde(default)]
    is_error: bool,
}

fn mcp_call_result(server: &str, tool: &str, result: CallToolResult) -> ToolResult {
    let data = json!({
        "server": server,
        "tool": tool,
        "content": result.content,
        "structured_content": result.structured_content,
        "is_error": result.is_error,
    });
    if result.is_error {
        tool_error_json_with_data(render_call_error(&data), data)
    } else {
        tool_ok_json(data)
    }
}

fn render_call_error(data: &Value) -> String {
    let content = data
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty());
    content.unwrap_or_else(|| "MCP tool returned an error".to_string())
}

fn tool_ok_json(data: Value) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": true,
        "data": data,
    }))
    .expect("MCP tool result JSON must serialize");
    ToolResult {
        ok: true,
        content,
        error: None,
        summary: None,
    }
}

fn tool_error_json(error: String) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": false,
        "error": error,
    }))
    .expect("MCP tool error JSON must serialize");
    ToolResult {
        ok: false,
        error: Some(error.clone()),
        content,
        summary: Some(ToolExecutionSummary::error(error)),
    }
}

fn tool_error_json_with_data(error: String, data: Value) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "ok": false,
        "error": error,
        "data": data,
    }))
    .expect("MCP tool error JSON must serialize");
    ToolResult {
        ok: false,
        error: Some(error.clone()),
        content,
        summary: Some(ToolExecutionSummary::error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::net::{TcpListener, TcpStream};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeTransport {
        requests: Vec<String>,
        responses: VecDeque<Value>,
        notifications: Vec<String>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                requests: Vec::new(),
                responses: VecDeque::from(responses),
                notifications: Vec::new(),
            }
        }
    }

    impl JsonRpcTransport for FakeTransport {
        fn request(
            &mut self,
            method: &str,
            _params: Value,
            _timeout: Duration,
        ) -> Result<Value, String> {
            self.requests.push(method.to_string());
            self.responses
                .pop_front()
                .ok_or_else(|| "missing fake response".to_string())
        }

        fn notify(
            &mut self,
            method: &str,
            _params: Value,
            _timeout: Duration,
        ) -> Result<(), String> {
            self.notifications.push(method.to_string());
            Ok(())
        }
    }

    #[test]
    fn builds_normalized_mcp_tool_names() {
        assert_eq!(
            build_tool_name("GitHub Server", "Create Issue!").as_deref(),
            Some("mcp__github_server__create_issue")
        );
        assert_eq!(build_tool_name("!!!", "tool"), None);
        assert_eq!(build_tool_name("server", "???"), None);
    }

    #[test]
    fn initialize_and_list_tools_handles_pagination() {
        let mut transport = FakeTransport::new(vec![
            json!({"serverInfo": {"name": "fake"}}),
            json!({
                "tools": [{
                    "name": "search",
                    "description": "Search docs",
                    "inputSchema": {"type": "object"}
                }],
                "nextCursor": "next"
            }),
            json!({
                "tools": [{
                    "name": "fetch",
                    "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}
                }]
            }),
        ]);

        let tools =
            initialize_and_list_tools(&mut transport, Duration::from_secs(1)).expect("list tools");

        assert_eq!(
            transport.requests,
            ["initialize", "tools/list", "tools/list"]
        );
        assert_eq!(transport.notifications, ["notifications/initialized"]);
        assert_eq!(tools.len(), 2);

        let mut emitted = BTreeSet::new();
        let mut diagnostics = Vec::new();
        let (definitions, lookup) =
            build_tool_definitions("Docs", &tools, &mut emitted, &mut diagnostics);

        assert!(diagnostics.is_empty());
        assert_eq!(definitions[0].function.name, "mcp__docs__search");
        assert_eq!(definitions[1].function.name, "mcp__docs__fetch");
        assert_eq!(lookup["mcp__docs__search"], "search");
    }

    #[test]
    fn call_tool_wraps_success_and_error_results() {
        let mut success_transport = FakeTransport::new(vec![json!({
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": {"value": 1}
        })]);

        let success = call_tool(
            &mut success_transport,
            "read",
            json!({"path": "a.txt"}),
            Duration::from_secs(1),
        )
        .expect("call tool");
        let result = mcp_call_result("fs", "read", success);
        assert!(result.ok);
        assert!(result.content.contains(r#""server":"fs""#));

        let mut error_transport = FakeTransport::new(vec![json!({
            "content": [{"type": "text", "text": "denied"}],
            "isError": true
        })]);
        let error = call_tool(
            &mut error_transport,
            "write",
            json!({"path": "a.txt"}),
            Duration::from_secs(1),
        )
        .expect("call error tool");
        let result = mcp_call_result("fs", "write", error);
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("denied"));
        assert!(result.content.contains(r#""is_error":true"#));
    }

    #[test]
    fn duplicate_mcp_tool_names_are_reported_as_diagnostics() {
        let tools = vec![
            ListedTool {
                name: "Read File".into(),
                description: None,
                input_schema: None,
            },
            ListedTool {
                name: "read-file".into(),
                description: None,
                input_schema: None,
            },
        ];
        let mut emitted = BTreeSet::new();
        let mut diagnostics = Vec::new();

        let (definitions, _) = build_tool_definitions("fs", &tools, &mut emitted, &mut diagnostics);

        assert_eq!(definitions.len(), 1);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("skipped duplicate tool"));
    }

    #[test]
    fn cross_server_duplicate_mcp_tool_names_are_reported_as_diagnostics() {
        let first_tools = vec![ListedTool {
            name: "read".into(),
            description: None,
            input_schema: None,
        }];
        let second_tools = vec![ListedTool {
            name: "read".into(),
            description: None,
            input_schema: None,
        }];
        let mut emitted = BTreeSet::new();
        let mut diagnostics = Vec::new();

        let (first, _) =
            build_tool_definitions("FS!", &first_tools, &mut emitted, &mut diagnostics);
        let (second, _) =
            build_tool_definitions("FS?", &second_tools, &mut emitted, &mut diagnostics);

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("duplicate MCP tool name"));
    }

    #[cfg(not(windows))]
    #[test]
    fn mcp_tool_cache_reuses_stdio_process_between_discoveries() {
        let fixture = fake_server(false, false);
        let cache = McpToolCache::new();

        let first = discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache);
        let second = discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache);

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(started_count(&fixture.marker), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn mcp_tool_cache_restarts_dead_stdio_process_on_next_discovery() {
        let fixture = fake_server(true, false);
        let cache = McpToolCache::new();

        let first = discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache);
        thread::sleep(Duration::from_millis(100));
        let second = discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache);

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(started_count(&fixture.marker), 2);
    }

    #[cfg(not(windows))]
    #[test]
    fn bad_mcp_server_is_skipped_without_blocking_good_server() {
        let fixture = fake_server(false, false);
        let mut bad = fixture.config.clone();
        bad.name = "bad".to_string();
        bad.command = "definitely-not-a-real-morrow-mcp-command".to_string();

        let cache = McpToolCache::new();
        let discovery = discover_tools(&fixture.root, &[bad, fixture.config.clone()], &cache);

        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("mcp server bad"));
    }

    #[cfg(not(windows))]
    #[test]
    fn stderr_tail_is_included_in_discovery_diagnostics() {
        let fixture = fake_server(false, true);
        let cache = McpToolCache::new();

        let discovery =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache);

        assert!(discovery.tools.is_empty());
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("MCP request tools/list timed out"));
        assert!(discovery.diagnostics[0].contains("tail-message"));
    }

    #[test]
    fn http_mcp_server_lists_and_calls_tools_with_session_headers() {
        let server = TestHttpServer::start(vec![
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestHttpResponse::accepted(),
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo text",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                    }]
                }
            })),
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {"content": [{"type": "text", "text": "called"}]}
            })),
        ]);
        let root = unique_dir("http-mcp");
        let config = http_config(
            "remote",
            server.url(),
            BTreeMap::from([
                ("Authorization".to_string(), "Bearer token".to_string()),
                ("X-Morrow".to_string(), "static".to_string()),
            ]),
        );
        let cache = McpToolCache::new();

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache);
        assert!(discovery.diagnostics.is_empty());
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(
            discovery.tools[0].definitions()[0].function.name,
            "mcp__remote__echo"
        );

        let execution = discovery.tools[0].execute(
            &ToolCall::function("call_1", "mcp__remote__echo", r#"{"text":"hello"}"#),
            None,
        );
        let ToolExecution::Completed(result) = execution else {
            panic!("expected completed MCP call");
        };
        assert!(result.ok);
        assert!(result.content.contains("called"));

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].body.contains(r#""method":"initialize""#));
        assert!(
            requests[1]
                .body
                .contains(r#""method":"notifications/initialized""#)
        );
        assert!(requests[2].body.contains(r#""method":"tools/list""#));
        assert!(requests[3].body.contains(r#""method":"tools/call""#));
        for request in &requests {
            assert_eq!(
                request.headers.get("accept").map(String::as_str),
                Some("application/json, text/event-stream")
            );
            assert_eq!(
                request
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some("2025-06-18")
            );
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer token")
            );
            assert_eq!(
                request.headers.get("x-morrow").map(String::as_str),
                Some("static")
            );
        }
        for request in &requests[1..] {
            assert_eq!(
                request.headers.get("mcp-session-id").map(String::as_str),
                Some("session-1")
            );
        }
    }

    #[test]
    fn http_mcp_tools_list_accepts_sse_response() {
        let server = TestHttpServer::start(vec![
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestHttpResponse::accepted(),
            TestHttpResponse::sse(format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {"tools": [{"name": "search", "inputSchema": {"type": "object"}}]}
                })
            )),
        ]);
        let root = unique_dir("http-mcp-sse");
        let config = http_config("remote", server.url(), BTreeMap::new());
        let cache = McpToolCache::new();

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache);

        assert!(discovery.diagnostics.is_empty());
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(
            discovery.tools[0].definitions()[0].function.name,
            "mcp__remote__search"
        );
    }

    #[test]
    fn http_mcp_reinitializes_once_after_session_expiry() {
        let server = TestHttpServer::start(vec![
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestHttpResponse::accepted(),
            TestHttpResponse::status(StatusCode::NOT_FOUND, "expired"),
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-2"),
            TestHttpResponse::accepted(),
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
            })),
        ]);
        let root = unique_dir("http-mcp-expired");
        let config = http_config("remote", server.url(), BTreeMap::new());
        let cache = McpToolCache::new();

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache);

        assert!(discovery.diagnostics.is_empty());
        assert_eq!(discovery.tools.len(), 1);
        let requests = server.requests();
        assert_eq!(requests.len(), 6);
        assert!(requests[2].body.contains(r#""method":"tools/list""#));
        assert_eq!(
            requests[2]
                .headers
                .get("mcp-session-id")
                .map(String::as_str),
            Some("session-1")
        );
        assert!(requests[3].body.contains(r#""method":"initialize""#));
        assert!(!requests[3].headers.contains_key("mcp-session-id"));
        assert!(requests[5].body.contains(r#""method":"tools/list""#));
        assert_eq!(
            requests[5]
                .headers
                .get("mcp-session-id")
                .map(String::as_str),
            Some("session-2")
        );
    }

    #[test]
    fn http_mcp_cache_reuses_session_between_discoveries() {
        let server = TestHttpServer::start(vec![
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestHttpResponse::accepted(),
            TestHttpResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
            })),
        ]);
        let root = unique_dir("http-mcp-cache");
        let config = http_config("remote", server.url(), BTreeMap::new());
        let cache = McpToolCache::new();

        let first = discover_tools(&root, std::slice::from_ref(&config), &cache);
        let second = discover_tools(&root, std::slice::from_ref(&config), &cache);

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(server.requests().len(), 3);
    }

    #[cfg(not(windows))]
    #[test]
    fn bad_http_mcp_server_is_skipped_without_blocking_good_server() {
        let http = TestHttpServer::start(vec![TestHttpResponse::status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "nope",
        )]);
        let good = fake_server(false, false);
        let bad = http_config("bad-http", http.url(), BTreeMap::new());
        let cache = McpToolCache::new();

        let discovery = discover_tools(&good.root, &[bad, good.config.clone()], &cache);

        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("mcp server bad-http"));
    }

    #[cfg(not(windows))]
    struct FakeServer {
        root: PathBuf,
        marker: PathBuf,
        config: McpServerConfig,
    }

    #[cfg(not(windows))]
    fn fake_server(exit_after_list: bool, hang_on_list: bool) -> FakeServer {
        let root = unique_dir("mcp");
        let script = root.join("fake-mcp.sh");
        let marker = root.join("started.txt");
        let exit_after_list = if exit_after_list { "1" } else { "0" };
        let hang_on_list = if hang_on_list { "1" } else { "0" };
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf 'started\n' >> '{}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"fake","version":"1"}}}}}}'
      ;;
    *'"method":"tools/list"'*)
      if [ "{}" = "1" ]; then
        printf '%s\n' 'tail-message' >&2
        sleep 3
      fi
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"echo","description":"Echo","inputSchema":{{"type":"object"}}}}]}}}}'
      if [ "{}" = "1" ]; then
        exit 0
      fi
      ;;
    *'"method":"tools/call"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"content":[{{"type":"text","text":"called"}}]}}}}'
      ;;
  esac
done
"#,
                marker.display(),
                hang_on_list,
                exit_after_list
            ),
        )
        .expect("write fake server");

        FakeServer {
            root: root.clone(),
            marker,
            config: McpServerConfig {
                name: "fake".to_string(),
                transport: McpTransport::Stdio,
                command: "sh".to_string(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
                cwd: None,
                url: None,
                http_headers: BTreeMap::new(),
                enabled: true,
                startup_timeout_sec: 1,
                tool_timeout_sec: 1,
            },
        }
    }

    #[cfg(not(windows))]
    fn started_count(marker: &Path) -> usize {
        fs::read_to_string(marker).expect("marker").lines().count()
    }

    fn http_config(
        name: &str,
        url: String,
        http_headers: BTreeMap<String, String>,
    ) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some(url),
            http_headers,
            enabled: true,
            startup_timeout_sec: 5,
            tool_timeout_sec: 5,
        }
    }

    #[derive(Clone)]
    struct TestHttpRequest {
        headers: BTreeMap<String, String>,
        body: String,
    }

    struct TestHttpServer {
        addr: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<TestHttpRequest>>>,
    }

    impl TestHttpServer {
        fn start(responses: Vec<TestHttpResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let addr = listener.local_addr().expect("test server addr");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = requests.clone();
            thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept test request");
                    server_requests
                        .lock()
                        .expect("request lock")
                        .push(read_http_request(&mut stream));
                    stream
                        .write_all(response.as_http().as_bytes())
                        .expect("write test response");
                }
            });
            Self { addr, requests }
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }

        fn requests(&self) -> Vec<TestHttpRequest> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    struct TestHttpResponse {
        status: StatusCode,
        headers: BTreeMap<String, String>,
        body: String,
    }

    impl TestHttpResponse {
        fn json(body: Value) -> Self {
            Self {
                status: StatusCode::OK,
                headers: BTreeMap::from([(
                    "Content-Type".to_string(),
                    "application/json".to_string(),
                )]),
                body: body.to_string(),
            }
        }

        fn sse(body: String) -> Self {
            Self {
                status: StatusCode::OK,
                headers: BTreeMap::from([(
                    "Content-Type".to_string(),
                    "text/event-stream".to_string(),
                )]),
                body,
            }
        }

        fn accepted() -> Self {
            Self {
                status: StatusCode::ACCEPTED,
                headers: BTreeMap::new(),
                body: String::new(),
            }
        }

        fn status(status: StatusCode, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: BTreeMap::new(),
                body: body.into(),
            }
        }

        fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.headers.insert(name.into(), value.into());
            self
        }

        fn as_http(&self) -> String {
            let reason = self.status.canonical_reason().unwrap_or("status");
            let mut response = format!(
                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                self.status.as_u16(),
                reason,
                self.body.len()
            );
            for (name, value) in &self.headers {
                response.push_str(name);
                response.push_str(": ");
                response.push_str(value);
                response.push_str("\r\n");
            }
            response.push_str("\r\n");
            response.push_str(&self.body);
            response
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> TestHttpRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("read test request");
            assert!(read > 0, "test request closed before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = find_header_end(&bytes) {
                break index;
            }
        };
        let headers_text = String::from_utf8_lossy(&bytes[..header_end]);
        let mut headers = BTreeMap::new();
        for line in headers_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let read = stream.read(&mut buffer).expect("read test body");
            assert!(read > 0, "test request closed before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body =
            String::from_utf8_lossy(&bytes[body_start..body_start + content_length]).to_string();
        TestHttpRequest { headers, body }
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-mcp-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
