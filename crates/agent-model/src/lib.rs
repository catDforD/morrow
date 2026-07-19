pub use agent_core::ModelEvent;
use agent_core::{Model, ModelFailure, ModelFuture, ModelRequest, ModelStream};
use agent_protocol::{
    Conversation, Message, ReasoningLevel, ReasoningProfile, ToolCall, ToolDefinition,
};
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
}

impl std::fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatConfig")
            .field("base_url", &"<configured>")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    config: OpenAiCompatConfig,
    request_options: OpenAiCompatRequestOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiCompatRequestOptions {
    pub reasoning_profile: ReasoningProfile,
    pub reasoning: ReasoningLevel,
    pub supports_tools: bool,
}

impl Default for OpenAiCompatRequestOptions {
    fn default() -> Self {
        Self {
            reasoning_profile: ReasoningProfile::None,
            reasoning: ReasoningLevel::Off,
            supports_tools: true,
        }
    }
}

impl std::fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
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
    #[error("model response was incomplete: finish_reason={0}")]
    IncompleteResponse(String),
    #[error("model returned an unsupported finish_reason: {0}")]
    UnsupportedFinishReason(String),
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "is_empty_tools")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ThinkingRequest {
    #[serde(rename = "type")]
    kind: &'static str,
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
    reasoning_content: Option<String>,
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

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelDescription>,
}

#[derive(Debug, Deserialize)]
struct ModelDescription {
    id: String,
}

impl OpenAiCompatClient {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, ModelError> {
        Self::build(config, false)
    }

    pub fn new_without_proxy(config: OpenAiCompatConfig) -> Result<Self, ModelError> {
        Self::build(config, true)
    }

    fn build(config: OpenAiCompatConfig, disable_proxy: bool) -> Result<Self, ModelError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(config.timeout)
            .read_timeout(config.timeout);
        if disable_proxy {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(ModelError::ClientBuild)?;
        Ok(Self {
            http,
            config,
            request_options: OpenAiCompatRequestOptions::default(),
        })
    }

    pub fn with_request_options(mut self, request_options: OpenAiCompatRequestOptions) -> Self {
        self.request_options = request_options;
        self
    }

    pub async fn stream_chat(
        &self,
        conversation: &Conversation,
        tools: &[ToolDefinition],
    ) -> Result<ChatCompletionStream, ModelError> {
        let messages = request_messages(conversation, self.request_options.reasoning_profile);
        let tools = if self.request_options.supports_tools {
            tools
        } else {
            &[]
        };
        let (thinking, reasoning_effort) = reasoning_request_options(self.request_options);
        let request = ChatCompletionRequest {
            model: &self.config.model,
            messages: &messages,
            stream: true,
            tools,
            tool_choice: (!tools.is_empty()).then_some("auto"),
            thinking,
            reasoning_effort,
        };
        let response = self
            .http
            .post(self.chat_completions_url())
            .bearer_auth(&self.config.api_key)
            .json(&request)
            .send()
            .await
            .map_err(|error| ModelError::Request(error.without_url()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|error| {
                format!("failed to read error body: {}", error.without_url())
            });
            return Err(ModelError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        Ok(ChatCompletionStream::new(response.bytes_stream().boxed()))
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ModelError> {
        let response = self
            .http
            .get(self.models_url())
            .bearer_auth(&self.config.api_key)
            .send()
            .await
            .map_err(|error| ModelError::Request(error.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|error| {
                format!("failed to read error body: {}", error.without_url())
            });
            return Err(ModelError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        let mut models = response
            .json::<ModelsResponse>()
            .await
            .map_err(|error| ModelError::Request(error.without_url()))?
            .data
            .into_iter()
            .map(|model| model.id)
            .filter(|id| !id.trim().is_empty())
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        Ok(models)
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.config.base_url.trim_end_matches('/'))
    }
}

fn request_messages(
    conversation: &Conversation,
    reasoning_profile: ReasoningProfile,
) -> Vec<Message> {
    let mut messages = conversation.messages.clone();
    if reasoning_profile == ReasoningProfile::None {
        for message in &mut messages {
            message.reasoning_content = None;
        }
    }
    messages
}

fn reasoning_request_options(
    options: OpenAiCompatRequestOptions,
) -> (Option<ThinkingRequest>, Option<&'static str>) {
    if options.reasoning_profile != ReasoningProfile::Deepseek {
        return (None, None);
    }

    match options.reasoning {
        ReasoningLevel::Off => (Some(ThinkingRequest { kind: "disabled" }), None),
        ReasoningLevel::High => (Some(ThinkingRequest { kind: "enabled" }), Some("high")),
        ReasoningLevel::Max => (Some(ThinkingRequest { kind: "enabled" }), Some("max")),
    }
}

impl Model for OpenAiCompatClient {
    fn stream(&self, request: ModelRequest) -> ModelFuture {
        let client = self.clone();
        async move {
            let stream =
                OpenAiCompatClient::stream_chat(&client, &request.conversation, &request.tools)
                    .await
                    .map_err(ModelFailure::new)?;
            let stream: ModelStream = stream.map(|event| event.map_err(ModelFailure::new)).boxed();
            Ok(stream)
        }
        .boxed()
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
            self.finish_with_completion();
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

            if let Some(reasoning_content) = choice.delta.reasoning_content
                && !reasoning_content.is_empty()
            {
                self.pending
                    .push_back(Ok(ModelEvent::ReasoningDelta(reasoning_content)));
            }

            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                self.saw_text = true;
                self.pending.push_back(Ok(ModelEvent::TextDelta(content)));
            }

            match choice.finish_reason.as_deref() {
                Some("tool_calls") => {
                    self.finish_with_tool_calls();
                    return;
                }
                Some("stop") => {
                    self.finish_with_completion();
                    return;
                }
                Some(reason @ ("length" | "content_filter")) => {
                    self.finish_with_error(ModelError::IncompleteResponse(reason.to_string()));
                    return;
                }
                Some(reason) => {
                    self.finish_with_error(ModelError::UnsupportedFinishReason(reason.to_string()));
                    return;
                }
                None => {}
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

    fn finish_with_completion(&mut self) {
        if self.saw_text {
            self.pending.push_back(Ok(ModelEvent::Completed));
        } else {
            self.pending.push_back(Err(ModelError::EmptyResponse));
        }
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
                    let message = if err.is_timeout() {
                        "timed out while waiting for model stream data".to_string()
                    } else {
                        err.without_url().to_string()
                    };
                    return Poll::Ready(Some(Err(ModelError::Stream(message))));
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

    async fn spawn_delayed_stream(chunks: Vec<(Duration, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write response headers");
            for (delay, chunk) in chunks {
                tokio::time::sleep(delay).await;
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        format!("http://{addr}/v1")
    }

    fn conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.push(Message::user("hello"));
        conversation
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let config = OpenAiCompatConfig {
            base_url: "https://example.com/v1?token=url-secret".to_string(),
            model: "test-model".to_string(),
            api_key: "model-secret".to_string(),
            timeout: Duration::from_secs(5),
        };
        let client = OpenAiCompatClient::new_without_proxy(config.clone()).expect("client");

        assert!(!format!("{config:?}").contains("model-secret"));
        assert!(!format!("{config:?}").contains("url-secret"));
        assert!(!format!("{client:?}").contains("model-secret"));
        assert!(!format!("{client:?}").contains("url-secret"));
    }

    #[tokio::test]
    async fn request_errors_redact_url_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind unused address");
        let addr = listener.local_addr().expect("unused address");
        drop(listener);
        let secret = "model-query-secret";
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url: format!("http://{addr}/v1?token={secret}"),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(1),
        })
        .expect("client");

        let Err(error) = client.stream_chat(&conversation(), &[]).await else {
            panic!("closed address must fail");
        };
        let message = error.to_string();

        assert!(message.contains("failed to send model request"));
        assert!(!message.contains(secret));
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
    async fn configured_timeout_allows_active_streams_to_run_longer_than_the_interval() {
        let base_url = spawn_delayed_stream(vec![
            (
                Duration::from_millis(100),
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first\"},\"finish_reason\":null}]}\n\n",
            ),
            (
                Duration::from_millis(100),
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"second\"},\"finish_reason\":null}]}\n\n",
            ),
            (
                Duration::from_millis(100),
                "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
            ),
            (Duration::from_millis(100), "data: [DONE]\n\n"),
        ])
        .await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_millis(250),
        })
        .expect("client");

        let events = collect_events(
            client
                .stream_chat(&conversation(), &[])
                .await
                .expect("stream chat"),
        )
        .await;

        assert!(
            matches!(
                events.as_slice(),
                [
                    Ok(ModelEvent::ReasoningDelta(first)),
                    Ok(ModelEvent::ReasoningDelta(second)),
                    Ok(ModelEvent::TextDelta(answer)),
                    Ok(ModelEvent::Completed),
                ] if first == "first" && second == "second" && answer == "answer"
            ),
            "unexpected events: {events:?}"
        );
    }

    #[tokio::test]
    async fn configured_timeout_still_rejects_stalled_streams() {
        let base_url = spawn_delayed_stream(vec![
            (
                Duration::from_millis(20),
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            ),
            (Duration::from_millis(300), "data: [DONE]\n\n"),
        ])
        .await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_millis(100),
        })
        .expect("client");

        let events = collect_events(
            client
                .stream_chat(&conversation(), &[])
                .await
                .expect("stream chat"),
        )
        .await;

        assert!(matches!(
            events.as_slice(),
            [Ok(ModelEvent::TextDelta(text)), Err(ModelError::Stream(message))]
                if text == "partial"
                    && message == "timed out while waiting for model stream data"
        ));
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
    async fn parses_interleaved_reasoning_content_and_fragmented_tool_calls() {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({
                "choices": [{
                    "delta": {
                        "reasoning_content": "inspect first",
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
                        "content": "checking",
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

        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].as_ref().expect("reasoning"),
            &ModelEvent::ReasoningDelta("inspect first".to_string())
        );
        assert_eq!(
            events[1].as_ref().expect("content"),
            &ModelEvent::TextDelta("checking".to_string())
        );
        assert_eq!(
            events[2].as_ref().expect("tool calls"),
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
    async fn sends_deepseek_reasoning_controls_for_all_supported_levels() {
        for (reasoning, expected_type, expected_effort) in [
            (ReasoningLevel::Off, "disabled", None),
            (ReasoningLevel::High, "enabled", Some("high")),
            (ReasoningLevel::Max, "enabled", Some("max")),
        ] {
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                "data: [DONE]\n\n"
            );
            let (base_url, requests) = spawn_recording_server(body).await;
            let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
                base_url,
                model: "deepseek-v4-pro".to_string(),
                api_key: "test-key".to_string(),
                timeout: Duration::from_secs(5),
            })
            .expect("client")
            .with_request_options(OpenAiCompatRequestOptions {
                reasoning_profile: ReasoningProfile::Deepseek,
                reasoning,
                supports_tools: true,
            });

            let stream = client
                .stream_chat(&conversation(), &[])
                .await
                .expect("stream chat");
            let _ = collect_events(stream).await;

            let requests = requests.lock().expect("requests lock poisoned");
            assert!(requests[0].contains(&format!(r#""thinking":{{"type":"{expected_type}"}}"#)));
            match expected_effort {
                Some(effort) => {
                    assert!(requests[0].contains(&format!(r#""reasoning_effort":"{effort}""#)))
                }
                None => assert!(!requests[0].contains("reasoning_effort")),
            }
        }
    }

    #[tokio::test]
    async fn generic_provider_strips_reasoning_content_from_history() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, requests) = spawn_recording_server(body).await;
        let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
            base_url,
            model: "generic-model".to_string(),
            api_key: "test-key".to_string(),
            timeout: Duration::from_secs(5),
        })
        .expect("client");
        let mut conversation = Conversation::new();
        conversation.push(Message::assistant("answer").with_reasoning_content("private reasoning"));

        let stream = client
            .stream_chat(&conversation, &[])
            .await
            .expect("stream chat");
        let _ = collect_events(stream).await;

        let requests = requests.lock().expect("requests lock poisoned");
        assert!(!requests[0].contains("reasoning_content"));
        assert!(!requests[0].contains("private reasoning"));
    }

    #[tokio::test]
    async fn parses_reasoning_delta_before_answer_text() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let client = client_for(body).await;
        let events = collect_events(
            client
                .stream_chat(&conversation(), &[])
                .await
                .expect("stream chat"),
        )
        .await;

        assert!(matches!(
            events.as_slice(),
            [
                Ok(ModelEvent::ReasoningDelta(reasoning)),
                Ok(ModelEvent::TextDelta(text)),
                Ok(ModelEvent::Completed),
            ] if reasoning == "think" && text == "answer"
        ));
    }

    #[tokio::test]
    async fn stop_finish_reason_completes_without_done_sentinel() {
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\n";
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert!(matches!(
            events.as_slice(),
            [Ok(ModelEvent::TextDelta(text)), Ok(ModelEvent::Completed)] if text == "Hi"
        ));
    }

    #[tokio::test]
    async fn length_finish_reason_is_reported_as_incomplete() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\n";
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert!(matches!(
            events.as_slice(),
            [
                Ok(ModelEvent::TextDelta(text)),
                Err(ModelError::IncompleteResponse(reason)),
            ] if text == "partial" && reason == "length"
        ));
    }

    #[tokio::test]
    async fn unknown_finish_reason_is_reported_explicitly() {
        let body =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"provider_specific\"}]}\n\n";
        let client = client_for(body).await;
        let stream = client
            .stream_chat(&conversation(), &[])
            .await
            .expect("stream chat");

        let events = collect_events(stream).await;

        assert!(matches!(
            events.as_slice(),
            [Err(ModelError::UnsupportedFinishReason(reason))]
                if reason == "provider_specific"
        ));
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
