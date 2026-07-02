use agent_protocol::{Conversation, ToolCall, ToolDefinition};
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    config: OpenAiCompatConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    TextDelta(String),
    ToolCalls(Vec<ToolCall>),
    Completed,
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("failed to build HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    #[error("model provider returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("failed to send model request: {0}")]
    Request(#[source] reqwest::Error),
    #[error("failed to read model stream: {0}")]
    Stream(String),
    #[error("model stream was not valid UTF-8: {0}")]
    Utf8(String),
    #[error("failed to parse model stream JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("model stream ended before data: [DONE]")]
    StreamEndedBeforeDone,
    #[error("model returned an empty answer")]
    EmptyResponse,
    #[error("model requested a tool call, but tools are not supported in this version")]
    UnsupportedToolCall,
    #[error("model returned an invalid tool call: {0}")]
    InvalidToolCall(String),
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [agent_protocol::Message],
    stream: bool,
    #[serde(skip_serializing_if = "is_empty_tools")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

fn is_empty_tools(tools: &[ToolDefinition]) -> bool {
    tools.is_empty()
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    delta: ChatCompletionDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCompletionDelta {
    content: Option<String>,
    tool_calls: Option<Vec<ChatCompletionToolCallDelta>>,
    function_call: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<ChatCompletionFunctionCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionFunctionCallDelta {
    name: Option<String>,
    arguments: Option<String>,
}

impl OpenAiCompatClient {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, ModelError> {
        Self::build(config, false)
    }

    pub fn new_without_proxy(config: OpenAiCompatConfig) -> Result<Self, ModelError> {
        Self::build(config, true)
    }

    fn build(config: OpenAiCompatConfig, disable_proxy: bool) -> Result<Self, ModelError> {
        let mut builder = reqwest::Client::builder().timeout(config.timeout);
        if disable_proxy {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(ModelError::ClientBuild)?;
        Ok(Self { http, config })
    }

    pub async fn stream_chat(
        &self,
        conversation: &Conversation,
        tools: &[ToolDefinition],
    ) -> Result<ChatCompletionStream, ModelError> {
        let request = ChatCompletionRequest {
            model: &self.config.model,
            messages: &conversation.messages,
            stream: true,
            tools,
            tool_choice: (!tools.is_empty()).then_some("auto"),
        };
        let response = self
            .http
            .post(self.chat_completions_url())
            .bearer_auth(&self.config.api_key)
            .json(&request)
            .send()
            .await
            .map_err(ModelError::Request)?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|err| format!("failed to read error body: {err}"));
            return Err(ModelError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        Ok(ChatCompletionStream::new(response.bytes_stream().boxed()))
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }
}

pub struct ChatCompletionStream {
    inner: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    buffer: Vec<u8>,
    pending: VecDeque<Result<ModelEvent, ModelError>>,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    done: bool,
    saw_text: bool,
}

impl ChatCompletionStream {
    fn new(inner: BoxStream<'static, Result<Bytes, reqwest::Error>>) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            tool_calls: BTreeMap::new(),
            done: false,
            saw_text: false,
        }
    }

    fn push_chunk(&mut self, bytes: Bytes) {
        self.buffer.extend_from_slice(&bytes);

        while let Some((frame_end, delimiter_len)) = find_sse_frame_end(&self.buffer) {
            let frame = self.buffer[..frame_end].to_vec();
            self.buffer.drain(..frame_end + delimiter_len);
            let frame = match String::from_utf8(frame) {
                Ok(frame) => frame.replace("\r\n", "\n"),
                Err(err) => {
                    self.finish_with_error(ModelError::Utf8(err.to_string()));
                    return;
                }
            };
            self.handle_frame(&frame);
            if self.done {
                break;
            }
        }
    }

    fn handle_frame(&mut self, frame: &str) {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|value| value.strip_prefix(' ').unwrap_or(value))
            .collect::<Vec<_>>()
            .join("\n");

        if data.trim().is_empty() {
            return;
        }

        if data.trim() == "[DONE]" {
            if self.saw_text {
                self.pending.push_back(Ok(ModelEvent::Completed));
            } else {
                self.pending.push_back(Err(ModelError::EmptyResponse));
            }
            self.done = true;
            return;
        }

        let chunk = match serde_json::from_str::<ChatCompletionChunk>(&data) {
            Ok(chunk) => chunk,
            Err(err) => {
                self.finish_with_error(ModelError::Json(err));
                return;
            }
        };

        for choice in chunk.choices {
            if choice.delta.function_call.is_some()
                || matches!(choice.finish_reason.as_deref(), Some("function_call"))
            {
                self.finish_with_error(ModelError::UnsupportedToolCall);
                return;
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tool_call in tool_calls {
                    self.accumulate_tool_call(tool_call);
                    if self.done {
                        return;
                    }
                }
            }

            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                self.saw_text = true;
                self.pending.push_back(Ok(ModelEvent::TextDelta(content)));
            }

            if matches!(choice.finish_reason.as_deref(), Some("tool_calls")) {
                self.finish_with_tool_calls();
                return;
            }
        }
    }

    fn accumulate_tool_call(&mut self, delta: ChatCompletionToolCallDelta) {
        if let Some(kind) = delta.kind.as_deref()
            && kind != "function"
        {
            self.finish_with_error(ModelError::InvalidToolCall(format!(
                "unsupported tool call type {kind:?}"
            )));
            return;
        }

        let accumulator = self.tool_calls.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            accumulator.id = Some(id);
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                accumulator.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                accumulator.arguments.push_str(&arguments);
            }
        }
    }

    fn finish_with_tool_calls(&mut self) {
        if self.tool_calls.is_empty() {
            self.finish_with_error(ModelError::InvalidToolCall(
                "finish_reason was tool_calls but no tool calls were streamed".to_string(),
            ));
            return;
        }

        let mut tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (index, accumulator) in std::mem::take(&mut self.tool_calls) {
            let id = match accumulator.id {
                Some(id) if !id.is_empty() => id,
                _ => {
                    self.finish_with_error(ModelError::InvalidToolCall(format!(
                        "tool call at index {index} is missing id"
                    )));
                    return;
                }
            };
            if accumulator.name.is_empty() {
                self.finish_with_error(ModelError::InvalidToolCall(format!(
                    "tool call {id} is missing function name"
                )));
                return;
            }
            tool_calls.push(ToolCall::function(
                id,
                accumulator.name,
                accumulator.arguments,
            ));
        }

        self.pending
            .push_back(Ok(ModelEvent::ToolCalls(tool_calls)));
        self.done = true;
    }

    fn finish_with_error(&mut self, error: ModelError) {
        self.pending.push_back(Err(error));
        self.done = true;
    }
}

fn find_sse_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if buffer[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl Unpin for ChatCompletionStream {}

impl Stream for ChatCompletionStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        if this.done {
            return Poll::Ready(None);
        }

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.push_chunk(bytes);
                    if let Some(event) = this.pending.pop_front() {
                        return Poll::Ready(Some(event));
                    }
                    if this.done {
                        return Poll::Ready(None);
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ModelError::Stream(err.to_string()))));
                }
                Poll::Ready(None) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ModelError::StreamEndedBeforeDone)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{Message, ToolCall, ToolDefinition};
    use futures_util::{StreamExt, stream};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_server(status: &str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}/v1")
    }

    async fn spawn_recording_server(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        tokio::spawn(async move {
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
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.push(Message::user("hello"));
        conversation
    }

    async fn client_for(body: &'static str) -> OpenAiCompatClient {
        let base_url = spawn_server("200 OK", body).await;
        OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client")
    }

    async fn collect_events(
        mut stream: ChatCompletionStream,
    ) -> Vec<Result<ModelEvent, ModelError>> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn parses_multiple_text_deltas_and_done() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].as_ref().expect("event"),
            &ModelEvent::TextDelta("Hel".to_string())
        );
        assert_eq!(
            events[1].as_ref().expect("event"),
            &ModelEvent::TextDelta("lo".to_string())
        );
        assert_eq!(events[2].as_ref().expect("event"), &ModelEvent::Completed);
    }

    #[tokio::test]
    async fn parses_unicode_split_across_byte_chunks() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes()
        .to_vec();
        let split_at = body
            .iter()
            .position(|byte| *byte == 0xe4)
            .expect("unicode byte")
            + 1;
        let chunks = vec![
            Bytes::copy_from_slice(&body[..split_at]),
            Bytes::copy_from_slice(&body[split_at..]),
        ];
        let inner = stream::iter(chunks.into_iter().map(Ok::<Bytes, reqwest::Error>)).boxed();
        let stream = ChatCompletionStream::new(inner);

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].as_ref().expect("text"),
            &ModelEvent::TextDelta("你".to_string())
        );
        assert_eq!(events[1].as_ref().expect("done"), &ModelEvent::Completed);
    }

    #[tokio::test]
    async fn parses_crlf_sse_frames() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\r\n\r\n",
            "data: [DONE]\r\n\r\n"
        );
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].as_ref().expect("text"),
            &ModelEvent::TextDelta("ok".to_string())
        );
        assert_eq!(events[1].as_ref().expect("done"), &ModelEvent::Completed);
    }

    #[tokio::test]
    async fn returns_http_status_errors_before_streaming() {
        let base_url = spawn_server("401 Unauthorized", "nope").await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "bad-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client");

        let err = match client.stream_chat(&conversation(), &[]).await {
            Ok(_) => panic!("stream_chat must fail"),
            Err(err) => err,
        };

        assert!(matches!(err, ModelError::HttpStatus { status: 401, .. }));
    }

    #[tokio::test]
    async fn malformed_json_is_reported() {
        let client = client_for("data: {not-json}\n\n").await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Err(ModelError::Json(_))));
    }

    #[tokio::test]
    async fn parses_fragmented_tool_calls() {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"pa"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "th\":\"Cargo.toml\"}"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
        );
        let body: &'static str = Box::leak(body.into_boxed_str());
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].as_ref().expect("tool calls"),
            &ModelEvent::ToolCalls(vec![ToolCall::function(
                "call_1",
                "read_file",
                r#"{"path":"Cargo.toml"}"#
            )])
        );
    }

    #[tokio::test]
    async fn sends_tools_and_auto_tool_choice_when_tools_are_available() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_server(body).await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client");
        let tools = vec![ToolDefinition::function(
            "read_file",
            "Read a file",
            json!({"type": "object", "properties": {}}),
        )];

        let stream = client
            .stream_chat(&conversation(), &tools)
            .await
            .expect("stream chat");
        let _ = collect_events(stream).await;

        let requests = requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(r#""tool_choice":"auto""#));
        assert!(requests[0].contains(r#""tools":[{"type":"function""#));
        assert!(requests[0].contains(r#""name":"read_file""#));
    }

    #[tokio::test]
    async fn stream_end_before_done_is_reported() {
        let client = client_for("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n").await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].as_ref().expect("text"),
            ModelEvent::TextDelta(text) if text == "Hi"
        ));
        assert!(matches!(events[1], Err(ModelError::StreamEndedBeforeDone)));
    }

    #[tokio::test]
    async fn done_without_text_is_reported_as_empty_response() {
        let client = client_for("data: [DONE]\n\n").await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Err(ModelError::EmptyResponse)));
    }
}
