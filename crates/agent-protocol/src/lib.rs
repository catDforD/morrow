use serde::{Deserialize, Serialize};

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
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepKind {
    ModelCall,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TurnStep {
    pub kind: TurnStepKind,
    pub status: TurnStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub error: Option<String>,
}

impl TurnStep {
    pub fn running(kind: TurnStepKind) -> Self {
        Self {
            kind,
            status: TurnStatus::Running,
            tool_name: None,
            tool_call_id: None,
            error: None,
        }
    }

    pub fn running_model_call() -> Self {
        Self::running(TurnStepKind::ModelCall)
    }

    pub fn running_tool_call(name: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: TurnStepKind::ToolCall,
            status: TurnStatus::Running,
            tool_name: Some(name.into()),
            tool_call_id: Some(id.into()),
            error: None,
        }
    }

    pub fn complete(&mut self) {
        self.status = TurnStatus::Completed;
        self.error = None;
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = TurnStatus::Failed;
        self.error = Some(error.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Turn {
    pub status: TurnStatus,
    pub user_message: Message,
    pub assistant_message: Option<Message>,
    pub steps: Vec<TurnStep>,
    pub error: Option<String>,
}

impl Turn {
    pub fn running(user_message: Message) -> Self {
        Self {
            status: TurnStatus::Running,
            user_message,
            assistant_message: None,
            steps: vec![TurnStep::running_model_call()],
            error: None,
        }
    }

    pub fn complete(&mut self, assistant_message: Message) {
        self.status = TurnStatus::Completed;
        self.assistant_message = Some(assistant_message);
        self.error = None;
        if let Some(step) = self.steps.last_mut() {
            step.complete();
        }
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.status = TurnStatus::Failed;
        self.error = Some(error.clone());
        if let Some(step) = self.steps.last_mut() {
            step.fail(error);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    TextDelta(String),
    AgentMessage(String),
    ToolCallStarted { id: String, name: String },
    ToolCallFinished { id: String, name: String, ok: bool },
    TurnCompleted,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_messages_in_openai_chat_shape() {
        let mut conversation = Conversation::with_system_prompt("You are helpful.");
        conversation.push(Message::user("Hello"));
        conversation.push(Message::assistant("Hi"));

        let value = serde_json::to_value(&conversation.messages).expect("serialize messages");

        assert_eq!(
            value,
            json!([
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ])
        );
    }

    #[test]
    fn thread_serializes_long_term_messages_without_system_prompt() {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));

        let value = serde_json::to_value(&thread).expect("serialize thread");

        assert_eq!(
            value,
            json!({
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi"}
                ]
            })
        );
    }

    #[test]
    fn thread_document_serializes_versioned_thread() {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));

        let document = ThreadDocument::new(thread.clone());
        let value = serde_json::to_value(&document).expect("serialize thread document");

        assert_eq!(
            value,
            json!({
                "schema_version": 2,
                "thread": {
                    "messages": [
                        {"role": "user", "content": "Hello"},
                        {"role": "assistant", "content": "Hi"}
                    ]
                }
            })
        );

        let decoded =
            serde_json::from_value::<ThreadDocument>(value).expect("deserialize thread document");

        assert_eq!(decoded.schema_version, THREAD_DOCUMENT_SCHEMA_VERSION);
        assert_eq!(decoded.thread, thread);
    }

    #[test]
    fn serializes_assistant_tool_call_and_tool_result_messages() {
        let tool_call = ToolCall::function("call_1", "read_file", r#"{"path":"Cargo.toml"}"#);
        let messages = vec![
            Message::assistant_tool_calls(vec![tool_call]),
            Message::tool_result("call_1", r#"{"ok":true}"#),
        ];

        let value = serde_json::to_value(&messages).expect("serialize messages");

        assert_eq!(
            value,
            json!([
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "content": "{\"ok\":true}",
                    "tool_call_id": "call_1"
                }
            ])
        );
    }

    #[test]
    fn turn_serializes_running_model_call_shape() {
        let turn = Turn::running(Message::user("Hello"));

        let value = serde_json::to_value(&turn).expect("serialize turn");

        assert_eq!(
            value,
            json!({
                "status": "running",
                "user_message": {"role": "user", "content": "Hello"},
                "assistant_message": null,
                "steps": [{
                    "kind": "model_call",
                    "status": "running",
                    "error": null
                }],
                "error": null
            })
        );
    }

    #[test]
    fn turn_records_completion_and_failure() {
        let mut completed = Turn::running(Message::user("Hello"));
        completed.complete(Message::assistant("Hi"));

        assert_eq!(completed.status, TurnStatus::Completed);
        assert_eq!(completed.assistant_message, Some(Message::assistant("Hi")));
        assert_eq!(completed.steps[0].status, TurnStatus::Completed);
        assert_eq!(completed.error, None);

        let mut failed = Turn::running(Message::user("Hello"));
        failed.fail("model error");

        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.assistant_message, None);
        assert_eq!(failed.steps[0].status, TurnStatus::Failed);
        assert_eq!(failed.steps[0].error, Some("model error".to_string()));
        assert_eq!(failed.error, Some("model error".to_string()));
    }
}
