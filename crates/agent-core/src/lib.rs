use agent_model::{ChatCompletionStream, ModelError, ModelEvent, OpenAiCompatClient};
use agent_protocol::{AgentEvent, Conversation, Message};
use futures_util::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Agent {
    client: OpenAiCompatClient,
    system_prompt: String,
    last_conversation: Arc<Mutex<Option<Conversation>>>,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("{0}")]
    Model(#[from] ModelError),
}

impl Agent {
    pub fn new(client: OpenAiCompatClient, system_prompt: impl Into<String>) -> Self {
        Self {
            client,
            system_prompt: system_prompt.into(),
            last_conversation: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run_once(&self, prompt: impl Into<String>) -> Result<AgentRunStream, AgentError> {
        let mut conversation = Conversation::with_system_prompt(self.system_prompt.clone());
        conversation.push(Message::user(prompt.into()));

        let model_stream = self.client.stream_chat(&conversation).await?;

        Ok(AgentRunStream {
            model_stream,
            conversation,
            last_conversation: Arc::clone(&self.last_conversation),
            assistant_text: String::new(),
            pending: VecDeque::from([AgentEvent::TurnStarted]),
            finished: false,
        })
    }

    pub fn last_conversation(&self) -> Option<Conversation> {
        self.last_conversation
            .lock()
            .expect("last conversation lock poisoned")
            .clone()
    }
}

pub struct AgentRunStream {
    model_stream: ChatCompletionStream,
    conversation: Conversation,
    last_conversation: Arc<Mutex<Option<Conversation>>>,
    assistant_text: String,
    pending: VecDeque<AgentEvent>,
    finished: bool,
}

impl AgentRunStream {
    fn complete_turn(&mut self) {
        let assistant_text = self.assistant_text.clone();
        self.conversation
            .push(Message::assistant(assistant_text.clone()));
        *self
            .last_conversation
            .lock()
            .expect("last conversation lock poisoned") = Some(self.conversation.clone());
        self.pending
            .push_back(AgentEvent::AgentMessage(assistant_text));
        self.pending.push_back(AgentEvent::TurnCompleted);
        self.finished = true;
    }

    fn fail_turn(&mut self, error: impl ToString) {
        self.pending.push_back(AgentEvent::Error(error.to_string()));
        self.finished = true;
    }
}

impl Unpin for AgentRunStream {}

impl Stream for AgentRunStream {
    type Item = AgentEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        if this.finished {
            return Poll::Ready(None);
        }

        match Pin::new(&mut this.model_stream).poll_next(cx) {
            Poll::Ready(Some(Ok(ModelEvent::TextDelta(text)))) => {
                this.assistant_text.push_str(&text);
                Poll::Ready(Some(AgentEvent::TextDelta(text)))
            }
            Poll::Ready(Some(Ok(ModelEvent::Completed))) => {
                this.complete_turn();
                Poll::Ready(this.pending.pop_front())
            }
            Poll::Ready(Some(Err(err))) => {
                this.fail_turn(err);
                Poll::Ready(this.pending.pop_front())
            }
            Poll::Ready(None) => {
                this.fail_turn("model stream ended before completion");
                Poll::Ready(this.pending.pop_front())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::OpenAiCompatConfig;
    use futures_util::StreamExt;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_sse_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn run_once_emits_events_and_records_conversation() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let base_url = spawn_sse_server(body).await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client");
        let agent = Agent::new(client, "You are helpful.");

        let mut stream = agent.run_once("Say hi").await.expect("run once");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }

        assert_eq!(
            events,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::TextDelta("Hello".to_string()),
                AgentEvent::TextDelta(" world".to_string()),
                AgentEvent::AgentMessage("Hello world".to_string()),
                AgentEvent::TurnCompleted,
            ]
        );

        let conversation = agent.last_conversation().expect("conversation recorded");
        assert_eq!(
            conversation.messages,
            vec![
                Message::system("You are helpful."),
                Message::user("Say hi"),
                Message::assistant("Hello world"),
            ]
        );
    }
}
