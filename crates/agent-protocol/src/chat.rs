use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls_with_content(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn with_reasoning_content(mut self, reasoning_content: impl Into<String>) -> Self {
        let reasoning_content = reasoning_content.into();
        self.reasoning_content = (!reasoning_content.is_empty()).then_some(reasoning_content);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: ToolDefinitionKind,
    pub function: ToolFunctionDefinition,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: ToolDefinitionKind::Function,
            function: ToolFunctionDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDefinitionKind {
    Function,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub function: ToolFunctionCall,
}

impl ToolCall {
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: ToolCallKind::Function,
            function: ToolFunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallKind {
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_system_prompt(system_prompt: impl Into<String>) -> Self {
        let mut conversation = Self::new();
        conversation.push(Message::system(system_prompt));
        conversation
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Thread {
    pub messages: Vec<Message>,
}

impl Thread {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

pub const THREAD_DOCUMENT_SCHEMA_VERSION: u32 = 2;
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ThreadDocument {
    pub schema_version: u32,
    pub thread: Thread,
}

impl ThreadDocument {
    pub fn new(thread: Thread) -> Self {
        Self {
            schema_version: THREAD_DOCUMENT_SCHEMA_VERSION,
            thread,
        }
    }
}

/// MCP 工具审批请求展示的序列化参数上限，避免超大参数淹没审批界面。
pub const MCP_ARGUMENTS_MAX_BYTES: usize = 2048;

pub(crate) fn truncate_mcp_arguments(arguments: &str) -> String {
    if arguments.len() <= MCP_ARGUMENTS_MAX_BYTES {
        return arguments.to_string();
    }
    let mut end = MCP_ARGUMENTS_MAX_BYTES;
    while !arguments.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &arguments[..end])
}
