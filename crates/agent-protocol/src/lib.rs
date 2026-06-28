use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
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

pub const THREAD_DOCUMENT_SCHEMA_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TurnStep {
    pub kind: TurnStepKind,
    pub status: TurnStatus,
    pub error: Option<String>,
}

impl TurnStep {
    pub fn running(kind: TurnStepKind) -> Self {
        Self {
            kind,
            status: TurnStatus::Running,
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
            steps: vec![TurnStep::running(TurnStepKind::ModelCall)],
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
                "schema_version": 1,
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
