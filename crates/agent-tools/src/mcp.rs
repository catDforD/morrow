use crate::{Tool, ToolExecution, ToolRegistryError, ToolResult};
use agent_config::McpServerConfig;
use agent_protocol::{ToolCall, ToolDefinition, ToolExecutionSummary};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub fn discover_stdio_tools(
    workspace_root: &Path,
    servers: &[McpServerConfig],
) -> Result<Vec<Arc<dyn Tool>>, ToolRegistryError> {
    let mut tools = Vec::new();
    for server in servers.iter().filter(|server| server.enabled) {
        let tool = discover_stdio_server(workspace_root, server).map_err(|message| {
            ToolRegistryError::McpServer {
                server: server.name.clone(),
                message,
            }
        })?;
        tools.push(tool as Arc<dyn Tool>);
    }
    Ok(tools)
}

fn discover_stdio_server(
    workspace_root: &Path,
    config: &McpServerConfig,
) -> Result<Arc<McpToolProvider>, String> {
    let startup_timeout = Duration::from_secs(config.startup_timeout_sec);
    let tool_timeout = Duration::from_secs(config.tool_timeout_sec);
    let cwd = resolve_cwd(workspace_root, config.cwd.as_deref());
    let mut transport = StdioTransport::start(config, cwd, startup_timeout)?;
    let listed_tools = initialize_and_list_tools(&mut transport, startup_timeout)?;
    let (definitions, lookup) = build_tool_definitions(&config.name, listed_tools)?;
    let runtime = Arc::new(McpServerRuntime {
        name: config.name.clone(),
        transport: Mutex::new(transport),
        tool_timeout,
    });

    Ok(Arc::new(McpToolProvider {
        runtime,
        definitions,
        lookup,
    }))
}

fn resolve_cwd(workspace_root: &Path, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => workspace_root.join(path),
        None => workspace_root.to_path_buf(),
    }
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
    transport: Mutex<StdioTransport>,
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
}

trait JsonRpcTransport {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String>;
}

struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Value, String>>,
    next_id: u64,
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
            .stderr(Stdio::null());

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
        let (tx, responses) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let message = match line {
                    Ok(line) if line.trim().is_empty() => continue,
                    Ok(line) => serde_json::from_str::<Value>(&line)
                        .map_err(|error| format!("failed to parse MCP JSON-RPC message: {error}")),
                    Err(error) => Err(format!("failed to read MCP server stdout: {error}")),
                };
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        let mut transport = Self {
            child,
            stdin,
            responses,
            next_id: 1,
        };
        transport.ensure_started(startup_timeout)?;
        Ok(transport)
    }

    fn ensure_started(&mut self, timeout: Duration) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Err(format!("MCP stdio server exited during startup: {status}")),
            Ok(None) => {
                if timeout.is_zero() {
                    Err("MCP startup timeout must be greater than zero".to_string())
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(format!("failed to inspect MCP server status: {error}")),
        }
    }

    fn write_message(&mut self, message: Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.stdin, &message)
            .map_err(|error| format!("failed to encode MCP JSON-RPC message: {error}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|error| format!("failed to write MCP JSON-RPC message: {error}"))?;
        self.stdin
            .flush()
            .map_err(|error| format!("failed to flush MCP JSON-RPC message: {error}"))
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
            let message = self
                .responses
                .recv_timeout(timeout)
                .map_err(|_| format!("MCP request {method} timed out"))??;
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("MCP request {method} failed: {error}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
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
    transport.notify("notifications/initialized", json!({}))?;
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

fn build_tool_definitions(
    server_name: &str,
    tools: Vec<ListedTool>,
) -> Result<(Vec<ToolDefinition>, HashMap<String, String>), String> {
    let mut names = BTreeSet::new();
    let mut definitions = Vec::with_capacity(tools.len());
    let mut lookup = HashMap::with_capacity(tools.len());
    for tool in tools {
        let normalized = build_tool_name(server_name, &tool.name).ok_or_else(|| {
            format!(
                "MCP tool name normalizes to empty: server={server_name}, tool={}",
                tool.name
            )
        })?;
        if !names.insert(normalized.clone()) {
            return Err(format!(
                "duplicate MCP tool name after normalization: {normalized}"
            ));
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
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        definitions.push(ToolDefinition::function(
            normalized.clone(),
            description,
            parameters,
        ));
        lookup.insert(normalized, tool.name);
    }
    Ok((definitions, lookup))
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

        fn notify(&mut self, method: &str, _params: Value) -> Result<(), String> {
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
        let (definitions, lookup) = build_tool_definitions("Docs", tools).expect("definitions");
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
    fn duplicate_mcp_tool_names_are_rejected() {
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

        let error = build_tool_definitions("fs", tools).expect_err("duplicate");

        assert!(error.contains("duplicate MCP tool name"));
    }
}
