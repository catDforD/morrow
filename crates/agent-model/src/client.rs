use super::*;

/// Default upper bound on chat completion attempts (the initial request plus
/// retries) when the caller does not configure one.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

const RETRY_BASE_BACKOFF: Duration = Duration::from_millis(500);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
    /// Maximum number of attempts per chat completion request, including the
    /// first one; `0` or `1` disables retries.
    pub max_retries: u32,
}

impl std::fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatConfig")
            .field("base_url", &"<configured>")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
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
    #[error("{source} (after {attempts} attempts)")]
    RetryExhausted {
        attempts: u32,
        #[source]
        source: Box<ModelError>,
    },
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

    /// Establishes a streaming chat completion, retrying transient failures
    /// (connect/timeout errors and HTTP 429/500/502/503/504) with exponential
    /// backoff. Failures after the stream has been established are *not*
    /// retried: deltas may already have been emitted, so retrying would risk
    /// duplicating content.
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
        let max_attempts = self.config.max_retries.max(1);
        let mut attempt = 0_u32;
        let response = loop {
            attempt += 1;
            match self.send_chat_completion(&request).await {
                Ok(response) => break response,
                Err(error) if attempt < max_attempts && is_retryable_request_error(&error) => {
                    tokio::time::sleep(retry_backoff(attempt)).await;
                }
                Err(error) => {
                    return Err(if attempt > 1 {
                        ModelError::RetryExhausted {
                            attempts: attempt,
                            source: Box::new(error),
                        }
                    } else {
                        error
                    });
                }
            }
        };

        Ok(ChatCompletionStream::new(response.bytes_stream().boxed()))
    }

    async fn send_chat_completion(
        &self,
        request: &ChatCompletionRequest<'_>,
    ) -> Result<reqwest::Response, ModelError> {
        let response = self
            .http
            .post(self.chat_completions_url())
            .bearer_auth(&self.config.api_key)
            .json(request)
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

        Ok(response)
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

/// Transient establish-phase failures worth retrying: connection/timeout
/// errors, rate limiting, and server-side statuses. Other 4xx responses
/// (auth, bad request) would fail identically on retry, so they are not
/// retried. Classification still works after `reqwest::Error::without_url()`.
fn is_retryable_request_error(error: &ModelError) -> bool {
    match error {
        ModelError::Request(source) => source.is_connect() || source.is_timeout(),
        ModelError::HttpStatus { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// Exponential backoff after the given 1-based failed attempt:
/// 500ms, 1s, 2s, ..., capped at 8s.
pub(crate) fn retry_backoff(failed_attempt: u32) -> Duration {
    let factor = 2_u32.saturating_pow(failed_attempt.saturating_sub(1).min(10));
    RETRY_BASE_BACKOFF
        .checked_mul(factor)
        .unwrap_or(RETRY_MAX_BACKOFF)
        .min(RETRY_MAX_BACKOFF)
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

    fn shared_clone(&self) -> Option<Arc<dyn Model>> {
        self.request_options
            .supports_tools
            .then(|| Arc::new(self.clone()) as Arc<dyn Model>)
    }
}
