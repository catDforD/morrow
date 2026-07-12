use crate::{TOOL_CANCELLED_ERROR, Tool, ToolExecution, ToolExecutionContext, ToolResult};
use agent_config::{McpServerConfig, McpTransport};
use agent_protocol::{ToolCall, ToolDefinition, ToolExecutionSummary};
use async_trait::async_trait;
use futures_util::future::join_all;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const STDERR_TAIL_BYTES: usize = 8192;
const MCP_ACTOR_QUEUE_CAPACITY: usize = 64;

#[derive(Default)]
pub struct McpToolCache {
    entries: tokio::sync::Mutex<HashMap<McpServerKey, McpCacheEntry>>,
}

impl McpToolCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn clear(&self) {
        self.entries.lock().await.clear();
    }

    async fn get_or_start(
        &self,
        config: &McpServerConfig,
        cwd: PathBuf,
    ) -> Result<Arc<CachedMcpServer>, String> {
        let key = McpServerKey::from_config(config, &cwd);

        loop {
            match self.cache_action(&key).await {
                McpCacheAction::Ready(entry) => {
                    if entry.runtime.is_healthy().await {
                        return Ok(entry);
                    }
                    self.evict_ready_if_current(&key, &entry).await;
                }
                McpCacheAction::Starting(mut wait) => {
                    self.wait_for_start(&key, &mut wait).await;
                }
                McpCacheAction::Start(signal) => {
                    let result = start_mcp_server(config, cwd.clone()).await;
                    self.finish_start(&key, signal, result.as_ref().ok().cloned())
                        .await;
                    return result;
                }
            }
        }
    }

    async fn cache_action(&self, key: &McpServerKey) -> McpCacheAction {
        let mut entries = self.entries.lock().await;
        match entries.get(key) {
            Some(McpCacheEntry::Ready(entry)) => McpCacheAction::Ready(entry.clone()),
            Some(McpCacheEntry::Starting(wait)) => McpCacheAction::Starting(wait.clone()),
            None => {
                let generation = Arc::new(());
                let (completed, receiver) = watch::channel(false);
                entries.insert(
                    key.clone(),
                    McpCacheEntry::Starting(McpStartWait {
                        generation: generation.clone(),
                        completed: receiver,
                    }),
                );
                McpCacheAction::Start(McpStartSignal {
                    generation,
                    completed,
                })
            }
        }
    }

    async fn wait_for_start(&self, key: &McpServerKey, wait: &mut McpStartWait) {
        if wait
            .completed
            .wait_for(|completed| *completed)
            .await
            .is_ok()
        {
            return;
        }

        // 启动任务可能被取消。只清理同一代启动状态，避免误删后来创建的新任务。
        let mut entries = self.entries.lock().await;
        if matches!(
            entries.get(key),
            Some(McpCacheEntry::Starting(current))
                if Arc::ptr_eq(&current.generation, &wait.generation)
        ) {
            entries.remove(key);
        }
    }

    async fn evict_ready_if_current(&self, key: &McpServerKey, stale: &Arc<CachedMcpServer>) {
        let mut entries = self.entries.lock().await;
        if matches!(
            entries.get(key),
            Some(McpCacheEntry::Ready(current)) if Arc::ptr_eq(current, stale)
        ) {
            entries.remove(key);
        }
    }

    async fn finish_start(
        &self,
        key: &McpServerKey,
        signal: McpStartSignal,
        entry: Option<Arc<CachedMcpServer>>,
    ) {
        let mut entries = self.entries.lock().await;
        if matches!(
            entries.get(key),
            Some(McpCacheEntry::Starting(current))
                if Arc::ptr_eq(&current.generation, &signal.generation)
        ) {
            entries.remove(key);
            if let Some(entry) = entry {
                entries.insert(key.clone(), McpCacheEntry::Ready(entry));
            }
        }
        drop(entries);

        // watch 会保留最后一个值，完成信号先于 waiter 开始等待也不会丢失。
        let _ = signal.completed.send(true);
    }
}

impl std::fmt::Debug for McpToolCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolCache")
            .field("entries", &"<async mutex>")
            .finish()
    }
}

enum McpCacheEntry {
    Ready(Arc<CachedMcpServer>),
    Starting(McpStartWait),
}

enum McpCacheAction {
    Ready(Arc<CachedMcpServer>),
    Starting(McpStartWait),
    Start(McpStartSignal),
}

#[derive(Clone)]
struct McpStartWait {
    generation: Arc<()>,
    completed: watch::Receiver<bool>,
}

struct McpStartSignal {
    generation: Arc<()>,
    completed: watch::Sender<bool>,
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
    runtime: McpServerRuntime,
    listed_tools: Vec<ListedTool>,
}

pub async fn discover_tools(
    workspace_root: &Path,
    servers: &[McpServerConfig],
    cache: &McpToolCache,
) -> McpDiscovery {
    let discoveries = join_all(
        servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| {
                let cwd = resolve_cwd(workspace_root, server.cwd.as_deref());
                async move { (server.name.clone(), cache.get_or_start(server, cwd).await) }
            }),
    )
    .await;

    let mut tools = Vec::new();
    let mut diagnostics = Vec::new();
    let mut emitted_names = BTreeSet::new();

    for (server_name, result) in discoveries {
        let entry = match result {
            Ok(entry) => entry,
            Err(message) => {
                diagnostics.push(format!("mcp server {server_name}: {message}"));
                continue;
            }
        };

        if let Some(tool) =
            build_tool_provider(&server_name, entry, &mut emitted_names, &mut diagnostics)
        {
            tools.push(tool as Arc<dyn Tool>);
        }
    }

    McpDiscovery { tools, diagnostics }
}

async fn start_mcp_server(
    config: &McpServerConfig,
    cwd: PathBuf,
) -> Result<Arc<CachedMcpServer>, String> {
    let startup_timeout = Duration::from_secs(config.startup_timeout_sec);
    let tool_timeout = Duration::from_secs(config.tool_timeout_sec);
    let mut transport = McpTransportClient::start(config, cwd, startup_timeout).await?;
    let listed_tools = initialize_and_list_tools(&mut transport, startup_timeout).await?;
    let runtime = McpServerRuntime::start(config.name.clone(), transport, tool_timeout);

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
    runtime: McpServerRuntime,
    definitions: Vec<ToolDefinition>,
    lookup: HashMap<String, String>,
}

#[async_trait]
impl Tool for McpToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    async fn execute(
        &self,
        call: ToolCall,
        _approval: Option<crate::ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolExecution {
        if context.cancellation.is_cancelled() {
            return ToolExecution::error(TOOL_CANCELLED_ERROR);
        }
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

        let result = tokio::select! {
            _ = context.cancellation.cancelled() => {
                return ToolExecution::error(TOOL_CANCELLED_ERROR);
            }
            result = self.runtime.call_tool(original_name, arguments) => result,
        };
        ToolExecution::Completed(match result {
            Ok(result) => mcp_call_result(&self.runtime.name, original_name, result),
            Err(error) => tool_error_json(error),
        })
    }
}

#[derive(Clone)]
struct McpServerRuntime {
    name: String,
    tx: mpsc::Sender<McpActorCommand>,
    healthy: Arc<AtomicBool>,
    tool_timeout: Duration,
}

impl McpServerRuntime {
    fn start(name: String, transport: McpTransportClient, tool_timeout: Duration) -> Self {
        let (tx, rx) = mpsc::channel(MCP_ACTOR_QUEUE_CAPACITY);
        let healthy = Arc::new(AtomicBool::new(true));
        let actor = McpServerActor {
            transport,
            rx,
            healthy: healthy.clone(),
        };
        tokio::spawn(actor.run());
        Self {
            name,
            tx,
            healthy,
            tool_timeout,
        }
    }

    async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<CallToolResult, String> {
        if !self.is_healthy().await {
            return Err("MCP server actor is not healthy".to_string());
        }
        let (reply, response) = oneshot::channel();
        self.tx
            .send(McpActorCommand::CallTool {
                tool_name: tool_name.to_string(),
                arguments,
                timeout: self.tool_timeout,
                reply,
            })
            .await
            .map_err(|_| "MCP server actor stopped".to_string())?;
        response
            .await
            .map_err(|_| "MCP server actor dropped response".to_string())?
    }

    async fn is_healthy(&self) -> bool {
        if !self.healthy.load(Ordering::SeqCst) || self.tx.is_closed() {
            return false;
        }
        let (reply, response) = oneshot::channel();
        if self
            .tx
            .send(McpActorCommand::Health { reply })
            .await
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }
}

enum McpActorCommand {
    Health {
        reply: oneshot::Sender<bool>,
    },
    CallTool {
        tool_name: String,
        arguments: Value,
        timeout: Duration,
        reply: oneshot::Sender<Result<CallToolResult, String>>,
    },
}

struct McpServerActor {
    transport: McpTransportClient,
    rx: mpsc::Receiver<McpActorCommand>,
    healthy: Arc<AtomicBool>,
}

impl McpServerActor {
    async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                McpActorCommand::Health { reply } => {
                    let healthy = self.transport.is_healthy().await;
                    let _ = reply.send(healthy);
                    if !healthy {
                        self.healthy.store(false, Ordering::SeqCst);
                        break;
                    }
                }
                McpActorCommand::CallTool {
                    tool_name,
                    arguments,
                    timeout,
                    reply,
                } => {
                    // 调用方可能在排队期间已取消；此时不要再启动远端副作用。
                    if reply.is_closed() {
                        continue;
                    }
                    let result =
                        call_tool(&mut self.transport, &tool_name, arguments, timeout).await;
                    let failed = result.is_err() && !self.transport.is_healthy().await;
                    let _ = reply.send(result);
                    if failed {
                        self.healthy.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        self.healthy.store(false, Ordering::SeqCst);
        self.transport.shutdown().await;
    }
}

#[async_trait]
trait JsonRpcTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String>;

    async fn notify(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), String>;

    async fn is_healthy(&mut self) -> bool {
        true
    }

    async fn shutdown(&mut self) {}
}

enum McpTransportClient {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl McpTransportClient {
    async fn start(
        config: &McpServerConfig,
        cwd: PathBuf,
        startup_timeout: Duration,
    ) -> Result<Self, String> {
        match config.transport {
            McpTransport::Stdio => StdioTransport::start(config, cwd, startup_timeout)
                .await
                .map(Self::Stdio),
            McpTransport::Http => HttpTransport::start(config).map(Self::Http),
        }
    }
}

#[async_trait]
impl JsonRpcTransport for McpTransportClient {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        match self {
            Self::Stdio(transport) => transport.request(method, params, timeout).await,
            Self::Http(transport) => transport.request(method, params, timeout).await,
        }
    }

    async fn notify(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), String> {
        match self {
            Self::Stdio(transport) => transport.notify(method, params, timeout).await,
            Self::Http(transport) => transport.notify(method, params, timeout).await,
        }
    }

    async fn shutdown(&mut self) {
        match self {
            Self::Stdio(transport) => transport.shutdown().await,
            Self::Http(transport) => transport.shutdown().await,
        }
    }

    async fn is_healthy(&mut self) -> bool {
        match self {
            Self::Stdio(transport) => transport.is_healthy().await,
            Self::Http(transport) => transport.is_healthy().await,
        }
    }
}

struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    stderr_tail: Arc<Mutex<TailBuffer>>,
    next_id: u64,
    failed: bool,
}

impl StdioTransport {
    async fn start(
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

        spawn_stderr_tail(stderr, stderr_tail.clone());

        let mut transport = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
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

    async fn is_healthy(&mut self) -> bool {
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

    async fn write_message(&mut self, message: Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|error| format!("failed to encode MCP JSON-RPC message: {error}"))?;
        bytes.push(b'\n');
        if let Err(error) = self.stdin.write_all(&bytes).await {
            return Err(self.with_failed(format!("failed to write MCP JSON-RPC message: {error}")));
        }
        if let Err(error) = self.stdin.flush().await {
            return Err(self.with_failed(format!("failed to flush MCP JSON-RPC message: {error}")));
        }
        Ok(())
    }

    fn fail<T>(&mut self, message: String) -> Result<T, String> {
        self.failed = true;
        Err(self.with_stderr_tail(message))
    }

    fn with_failed(&mut self, message: String) -> String {
        self.failed = true;
        self.with_stderr_tail(message)
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
        let _ = self.child.start_kill();
    }
}

#[async_trait]
impl JsonRpcTransport for StdioTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        loop {
            let mut line = String::new();
            let read = match tokio::time::timeout(timeout, self.stdout.read_line(&mut line)).await {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => {
                    return self.fail(format!("failed to read MCP server stdout: {error}"));
                }
                Err(_) => return self.fail(format!("MCP request {method} timed out")),
            };
            if read == 0 {
                return self.fail(format!(
                    "MCP request {method} failed: MCP server stdout closed"
                ));
            };
            if line.trim().is_empty() {
                continue;
            }
            let message = serde_json::from_str::<Value>(&line).map_err(|error| {
                self.with_failed(format!(
                    "failed to parse MCP JSON-RPC message: {error}; line: {}",
                    line.trim_end()
                ))
            })?;

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

    async fn notify(
        &mut self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> Result<(), String> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn shutdown(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

struct HttpTransport {
    client: reqwest::Client,
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
        let client = reqwest::Client::builder()
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

    async fn request_message(
        &mut self,
        message: Value,
        id: u64,
        timeout: Duration,
    ) -> Result<Value, String> {
        let had_session = self.session_id.is_some();
        match self
            .post_jsonrpc(
                &message,
                HttpResponseKind::Request { expected_id: id },
                timeout,
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(HttpPostError::SessionExpired) if had_session => {
                self.reinitialize(timeout).await?;
                self.post_jsonrpc(
                    &message,
                    HttpResponseKind::Request { expected_id: id },
                    timeout,
                )
                .await
                .map_err(HttpPostError::into_message)
            }
            Err(error) => Err(error.into_message()),
        }
    }

    async fn reinitialize(&mut self, timeout: Duration) -> Result<(), String> {
        self.session_id = None;
        self.protocol_version = MCP_PROTOCOL_VERSION.to_string();
        let id = self.next_request_id();
        let message = initialize_request(id);
        let result = self
            .post_jsonrpc(
                &message,
                HttpResponseKind::Request { expected_id: id },
                timeout,
            )
            .await
            .map_err(HttpPostError::into_message)?;
        self.apply_initialize_result(&result);
        self.notify("notifications/initialized", json!({}), timeout)
            .await
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

    async fn post_jsonrpc(
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

        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                // URL 的 query/userinfo 可能包含 token，错误会进入 Warning/JSONL/WebSocket。
                HttpPostError::Failed("MCP HTTP request timed out".to_string())
            } else {
                HttpPostError::Failed(format!(
                    "failed to send MCP HTTP request: {}",
                    error.without_url()
                ))
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
        let body = response.text().await.map_err(|error| {
            HttpPostError::Failed(format!(
                "failed to read MCP HTTP body: {}",
                error.without_url()
            ))
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

#[async_trait]
impl JsonRpcTransport for HttpTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_request_id();
        if method == "initialize" {
            self.session_id = None;
            self.protocol_version = MCP_PROTOCOL_VERSION.to_string();
        }
        let result = self
            .request_message(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }),
                id,
                timeout,
            )
            .await?;
        if method == "initialize" {
            self.apply_initialize_result(&result);
        }
        Ok(result)
    }

    async fn notify(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), String> {
        self.post_jsonrpc(
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
            HttpResponseKind::Notification,
            timeout,
        )
        .await
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
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
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

async fn initialize_and_list_tools<T: JsonRpcTransport>(
    transport: &mut T,
    timeout: Duration,
) -> Result<Vec<ListedTool>, String> {
    transport
        .request(
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
        )
        .await?;
    transport
        .notify("notifications/initialized", json!({}), timeout)
        .await?;
    list_tools(transport, timeout).await
}

async fn list_tools<T: JsonRpcTransport>(
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
        let result = transport.request("tools/list", params, timeout).await?;
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

async fn call_tool<T: JsonRpcTransport>(
    transport: &mut T,
    tool_name: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<CallToolResult, String> {
    let result = transport
        .request(
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": arguments,
            }),
            timeout,
        )
        .await?;
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
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

    #[tokio::test]
    async fn concurrent_cache_waiter_observes_early_completion() {
        let root = unique_dir("mcp-cache-early-completion");
        let config = http_config(
            "remote",
            "http://127.0.0.1:1/mcp".to_string(),
            BTreeMap::new(),
        );
        let key = McpServerKey::from_config(&config, &root);
        let cache = Arc::new(McpToolCache::new());
        let McpCacheAction::Start(signal) = cache.cache_action(&key).await else {
            panic!("first caller must start the MCP server");
        };

        let (waiter_ready_tx, waiter_ready_rx) = oneshot::channel();
        let (release_waiter_tx, release_waiter_rx) = oneshot::channel();
        let waiter_cache = cache.clone();
        let waiter_key = key.clone();
        let waiter = tokio::spawn(async move {
            let McpCacheAction::Starting(mut wait) = waiter_cache.cache_action(&waiter_key).await
            else {
                panic!("concurrent caller must wait for the existing start");
            };
            waiter_ready_tx.send(()).expect("signal waiter ready");
            release_waiter_rx.await.expect("release waiter");
            waiter_cache.wait_for_start(&waiter_key, &mut wait).await;
        });

        waiter_ready_rx
            .await
            .expect("waiter reached starting state");
        cache.finish_start(&key, signal, None).await;
        release_waiter_tx.send(()).expect("release waiter");

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must observe the completion sent before it waited")
            .expect("waiter task");
        assert!(!cache.entries.lock().await.contains_key(&key));
    }

    #[tokio::test]
    async fn clearing_cache_removes_in_progress_entries() {
        let root = unique_dir("mcp-cache-clear");
        let config = http_config(
            "remote",
            "http://127.0.0.1:1/mcp".to_string(),
            BTreeMap::new(),
        );
        let key = McpServerKey::from_config(&config, &root);
        let cache = McpToolCache::new();
        let McpCacheAction::Start(_signal) = cache.cache_action(&key).await else {
            panic!("first caller must start the MCP server");
        };

        cache.clear().await;

        assert!(cache.entries.lock().await.is_empty());
    }

    #[tokio::test]
    async fn stale_ready_check_does_not_remove_a_new_start_generation() {
        let root = unique_dir("mcp-cache-ready-generation");
        let config = http_config(
            "remote",
            "http://127.0.0.1:1/mcp".to_string(),
            BTreeMap::new(),
        );
        let key = McpServerKey::from_config(&config, &root);
        let cache = McpToolCache::new();
        let (tx, _rx) = mpsc::channel(MCP_ACTOR_QUEUE_CAPACITY);
        let stale = Arc::new(CachedMcpServer {
            runtime: McpServerRuntime {
                name: "stale".to_string(),
                tx,
                healthy: Arc::new(AtomicBool::new(false)),
                tool_timeout: Duration::from_secs(1),
            },
            listed_tools: Vec::new(),
        });
        let generation = Arc::new(());
        let (_completed, receiver) = watch::channel(false);
        cache.entries.lock().await.insert(
            key.clone(),
            McpCacheEntry::Starting(McpStartWait {
                generation: generation.clone(),
                completed: receiver,
            }),
        );

        cache.evict_ready_if_current(&key, &stale).await;

        let entries = cache.entries.lock().await;
        assert!(matches!(
            entries.get(&key),
            Some(McpCacheEntry::Starting(current))
                if Arc::ptr_eq(&current.generation, &generation)
        ));
    }

    #[tokio::test]
    async fn mcp_provider_returns_promptly_when_context_is_cancelled() {
        let (tx, mut rx) = mpsc::channel(MCP_ACTOR_QUEUE_CAPACITY);
        let runtime = McpServerRuntime {
            name: "slow".to_string(),
            tx,
            healthy: Arc::new(AtomicBool::new(true)),
            tool_timeout: Duration::from_secs(30),
        };
        let provider = McpToolProvider {
            runtime,
            definitions: Vec::new(),
            lookup: HashMap::from([("mcp__slow__wait".to_string(), "wait".to_string())]),
        };
        let cancellation = crate::CancellationToken::new();
        let context = ToolExecutionContext {
            cancellation: cancellation.clone(),
        };
        let (call_started_tx, call_started_rx) = oneshot::channel();
        let actor = tokio::spawn(async move {
            let Some(McpActorCommand::Health { reply }) = rx.recv().await else {
                panic!("expected health command");
            };
            reply.send(true).expect("health reply");

            let Some(McpActorCommand::CallTool { reply, .. }) = rx.recv().await else {
                panic!("expected tool call");
            };
            call_started_tx.send(()).expect("signal tool call");
            let _reply = reply;
            futures_util::future::pending::<()>().await;
        });

        let execution = tokio::spawn(async move {
            provider
                .execute(
                    ToolCall::function("call_1", "mcp__slow__wait", "{}"),
                    None,
                    context,
                )
                .await
        });
        call_started_rx.await.expect("tool call reached actor");
        cancellation.cancel();

        let execution = tokio::time::timeout(Duration::from_millis(250), execution)
            .await
            .expect("cancelled MCP call must return promptly")
            .expect("MCP execution task");
        let ToolExecution::Completed(result) = execution else {
            panic!("cancelled MCP call must complete with an error result");
        };
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some(TOOL_CANCELLED_ERROR));
        actor.abort();
    }

    #[async_trait]
    impl JsonRpcTransport for FakeTransport {
        async fn request(
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

        async fn notify(
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

    #[tokio::test]
    async fn initialize_and_list_tools_handles_pagination() {
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

        let tools = initialize_and_list_tools(&mut transport, Duration::from_secs(1))
            .await
            .expect("list tools");

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

    #[tokio::test]
    async fn call_tool_wraps_success_and_error_results() {
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
        .await
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
        .await
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
    #[tokio::test]
    async fn mcp_tool_cache_reuses_stdio_process_between_discoveries() {
        let fixture = fake_server(false, false);
        let cache = McpToolCache::new();

        let first =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache).await;
        let second =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache).await;

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(started_count(&fixture.marker), 1);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn mcp_tool_cache_restarts_dead_stdio_process_on_next_discovery() {
        let fixture = fake_server(true, false);
        let cache = McpToolCache::new();

        let first =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache).await;
        thread::sleep(Duration::from_millis(100));
        let second =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache).await;

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(started_count(&fixture.marker), 2);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn bad_mcp_server_is_skipped_without_blocking_good_server() {
        let fixture = fake_server(false, false);
        let mut bad = fixture.config.clone();
        bad.name = "bad".to_string();
        bad.command = "definitely-not-a-real-morrow-mcp-command".to_string();

        let cache = McpToolCache::new();
        let discovery = discover_tools(&fixture.root, &[bad, fixture.config.clone()], &cache).await;

        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("mcp server bad"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn stderr_tail_is_included_in_discovery_diagnostics() {
        let fixture = fake_server(false, true);
        let cache = McpToolCache::new();

        let discovery =
            discover_tools(&fixture.root, std::slice::from_ref(&fixture.config), &cache).await;

        assert!(discovery.tools.is_empty());
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("MCP request tools/list timed out"));
        assert!(discovery.diagnostics[0].contains("tail-message"));
    }

    #[tokio::test]
    async fn http_mcp_server_lists_and_calls_tools_with_session_headers() {
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

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache).await;
        assert!(discovery.diagnostics.is_empty());
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(
            discovery.tools[0].definitions()[0].function.name,
            "mcp__remote__echo"
        );

        let execution = discovery.tools[0]
            .execute(
                ToolCall::function("call_1", "mcp__remote__echo", r#"{"text":"hello"}"#),
                None,
                ToolExecutionContext::default(),
            )
            .await;
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

    #[tokio::test]
    async fn http_mcp_tools_list_accepts_sse_response() {
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

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache).await;

        assert!(discovery.diagnostics.is_empty());
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(
            discovery.tools[0].definitions()[0].function.name,
            "mcp__remote__search"
        );
    }

    #[tokio::test]
    async fn http_timeout_does_not_expose_url_query_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging server");
        let addr = listener.local_addr().expect("hanging server addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept hanging request");
            let _ = read_http_request(&mut stream);
            thread::sleep(Duration::from_millis(200));
        });
        let secret = "query-token-secret";
        let config = http_config(
            "remote",
            format!("http://{addr}/mcp?token={secret}"),
            BTreeMap::new(),
        );
        let mut transport = HttpTransport::start(&config).expect("HTTP transport");
        transport.client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client without proxy");

        let result = transport
            .post_jsonrpc(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                HttpResponseKind::Request { expected_id: 1 },
                Duration::from_millis(20),
            )
            .await;
        let Err(error) = result else {
            panic!("hanging server must time out");
        };
        let message = error.into_message();

        assert!(message.contains("timed out"));
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn http_connection_errors_do_not_expose_url_query_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused address");
        let addr = listener.local_addr().expect("unused address");
        drop(listener);
        let secret = "connection-query-secret";
        let config = http_config(
            "remote",
            format!("http://{addr}/mcp?token={secret}"),
            BTreeMap::new(),
        );
        let mut transport = HttpTransport::start(&config).expect("HTTP transport");
        transport.client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client without proxy");

        let result = transport
            .post_jsonrpc(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                HttpResponseKind::Request { expected_id: 1 },
                Duration::from_secs(1),
            )
            .await;
        let Err(error) = result else {
            panic!("closed address must fail");
        };
        let message = error.into_message();

        assert!(
            message.contains("failed to send MCP HTTP request"),
            "unexpected error: {message}"
        );
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn http_mcp_reinitializes_once_after_session_expiry() {
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

        let discovery = discover_tools(&root, std::slice::from_ref(&config), &cache).await;

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

    #[tokio::test]
    async fn http_mcp_cache_reuses_session_between_discoveries() {
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

        let first = discover_tools(&root, std::slice::from_ref(&config), &cache).await;
        let second = discover_tools(&root, std::slice::from_ref(&config), &cache).await;

        assert!(first.diagnostics.is_empty());
        assert!(second.diagnostics.is_empty());
        assert_eq!(first.tools.len(), 1);
        assert_eq!(second.tools.len(), 1);
        assert_eq!(server.requests().len(), 3);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn bad_http_mcp_server_is_skipped_without_blocking_good_server() {
        let http = TestHttpServer::start(vec![TestHttpResponse::status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "nope",
        )]);
        let good = fake_server(false, false);
        let bad = http_config("bad-http", http.url(), BTreeMap::new());
        let cache = McpToolCache::new();

        let discovery = discover_tools(&good.root, &[bad, good.config.clone()], &cache).await;

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
