use agent_model::{ChatCompletionStream, ModelError, ModelEvent, OpenAiCompatClient};
use agent_protocol::{AgentEvent, Conversation, Message, Thread, Turn};
use futures_util::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Agent {
    client: OpenAiCompatClient,
    system_prompt: String,
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
        }
    }

    pub async fn run_turn<'a>(
        &self,
        thread: &'a mut Thread,
        prompt: impl Into<String>,
    ) -> Result<AgentTurnStream<'a>, AgentError> {
        let user_message = Message::user(prompt.into());
        let mut conversation = Conversation::with_system_prompt(self.system_prompt.clone());
        conversation.messages.extend(thread.messages.clone());
        conversation.push(user_message.clone());

        let model_stream = self.client.stream_chat(&conversation).await?;

        Ok(AgentTurnStream {
            model_stream,
            thread,
            turn: Turn::running(user_message),
            assistant_text: String::new(),
            pending: VecDeque::from([AgentEvent::TurnStarted]),
            finished: false,
        })
    }
}

pub struct AgentTurnStream<'a> {
    model_stream: ChatCompletionStream,
    thread: &'a mut Thread,
    turn: Turn,
    assistant_text: String,
    pending: VecDeque<AgentEvent>,
    finished: bool,
}

impl AgentTurnStream<'_> {
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn into_turn(self) -> Turn {
        self.turn
    }

    fn complete_turn(&mut self) {
        let assistant_text = self.assistant_text.clone();
        let assistant_message = Message::assistant(assistant_text.clone());
        self.thread.push(self.turn.user_message.clone());
        self.thread.push(assistant_message.clone());
        self.turn.complete(assistant_message);
        self.pending
            .push_back(AgentEvent::AgentMessage(assistant_text));
        self.pending.push_back(AgentEvent::TurnCompleted);
        self.finished = true;
    }

    fn fail_turn(&mut self, error: impl ToString) {
        let error = error.to_string();
        self.turn.fail(error.clone());
        self.pending.push_back(AgentEvent::Error(error));
        self.finished = true;
    }
}

impl Unpin for AgentTurnStream<'_> {}

impl Stream for AgentTurnStream<'_> {
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
    use agent_protocol::TurnStatus;
    use futures_util::StreamExt;
    use std::sync::{Arc, Mutex};
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

    async fn spawn_recording_sse_server(
        bodies: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            for body in bodies {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 8192];
                let read = socket.read(&mut request).await.expect("read request");
                captured_requests
                    .lock()
                    .expect("requests lock poisoned")
                    .push(String::from_utf8_lossy(&request[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn client(base_url: String) -> OpenAiCompatClient {
        OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client")
    }

    async fn collect_events(mut stream: AgentTurnStream<'_>) -> (Vec<AgentEvent>, Turn) {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        let turn = stream.into_turn();
        (events, turn)
    }

    #[tokio::test]
    async fn run_turn_emits_events_and_updates_thread() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let base_url = spawn_sse_server(body).await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "Say hi")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

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

        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(
            turn.assistant_message,
            Some(Message::assistant("Hello world"))
        );
        assert_eq!(turn.steps[0].status, TurnStatus::Completed);
        assert_eq!(
            thread.messages,
            vec![Message::user("Say hi"), Message::assistant("Hello world"),]
        );
    }

    #[tokio::test]
    async fn run_turn_sends_prior_thread_messages_to_second_model_call() {
        let first_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"First answer\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let second_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Second answer\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_sse_server(vec![first_body, second_body]).await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();

        let stream = agent
            .run_turn(&mut thread, "First question")
            .await
            .expect("first turn");
        let _ = collect_events(stream).await;
        let stream = agent
            .run_turn(&mut thread, "Second question")
            .await
            .expect("second turn");
        let _ = collect_events(stream).await;

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(r#""content":"First question""#));
        assert!(!requests[0].contains(r#""content":"First answer""#));
        assert!(requests[1].contains(r#""content":"You are helpful.""#));
        assert!(requests[1].contains(r#""content":"First question""#));
        assert!(requests[1].contains(r#""content":"First answer""#));
        assert!(requests[1].contains(r#""content":"Second question""#));
        assert_eq!(
            thread.messages,
            vec![
                Message::user("First question"),
                Message::assistant("First answer"),
                Message::user("Second question"),
                Message::assistant("Second answer"),
            ]
        );
    }

    #[tokio::test]
    async fn failed_turn_emits_error_and_does_not_update_thread() {
        let base_url = spawn_sse_server("data: {not-json}\n\n").await;
        let agent = Agent::new(client(base_url), "You are helpful.");
        let mut thread = Thread::new();
        thread.push(Message::user("Earlier question"));
        thread.push(Message::assistant("Earlier answer"));

        let stream = agent
            .run_turn(&mut thread, "Broken question")
            .await
            .expect("run turn");
        let (events, turn) = collect_events(stream).await;

        assert_eq!(events.len(), 2);
        assert_eq!(events[0], AgentEvent::TurnStarted);
        assert!(matches!(events[1], AgentEvent::Error(_)));
        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(turn.error.is_some());
        assert_eq!(turn.steps[0].status, TurnStatus::Failed);
        assert_eq!(
            thread.messages,
            vec![
                Message::user("Earlier question"),
                Message::assistant("Earlier answer"),
            ]
        );
    }
}
