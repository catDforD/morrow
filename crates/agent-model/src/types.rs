use super::*;

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) messages: &'a [Message],
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "is_empty_tools")]
    pub(crate) tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<ThinkingRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ThinkingRequest {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
}

fn is_empty_tools(tools: &[ToolDefinition]) -> bool {
    tools.is_empty()
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionChunk {
    pub(crate) choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionChoice {
    pub(crate) delta: ChatCompletionDelta,
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatCompletionDelta {
    pub(crate) reasoning_content: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Option<Vec<ChatCompletionToolCallDelta>>,
    pub(crate) function_call: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionToolCallDelta {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) function: Option<ChatCompletionFunctionCallDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionFunctionCallDelta {
    pub(crate) name: Option<String>,
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelsResponse {
    pub(crate) data: Vec<ModelDescription>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelDescription {
    pub(crate) id: String,
}
