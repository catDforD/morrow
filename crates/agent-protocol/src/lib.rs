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
}
