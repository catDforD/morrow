//! 上下文 token 估算原语。
//!
//! 这是 runtime（turn 开始前压缩判定）与 core（turn 内水位护栏）共用的同一套账：
//! 每条消息固定基元开销 + 文本估算（ASCII 约 4 字符 1 token，非 ASCII 每字符 1 token），
//! 最后在请求级上浮 4/3 覆盖序列化与 envelope 开销。它是稳定的启发式估算，
//! 用于水位控制与回归检测，不是计费用途。

use agent_protocol::{Conversation, Message, Role, ToolDefinition};

/// 每条消息的固定基元开销（role 标签与分隔符的近似）。
pub const MESSAGE_BASE_TOKENS: usize = 6;
/// 一次 tool_calls 数组的固定基元开销。
pub const TOOL_CALL_BASE_TOKENS: usize = 12;
/// 请求级 padding 分子：估算在原始文本量上上浮 4/3。
pub const REQUEST_PADDING_NUMERATOR: usize = 4;
/// 请求级 padding 分母。
pub const REQUEST_PADDING_DENOMINATOR: usize = 3;

/// 估算一段文本的 token 量：ASCII 字符每 4 个算 1 token，非 ASCII 每字符 1 token。
pub fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    let mut ascii_chars = 0usize;
    let mut non_ascii_tokens = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_tokens += 1;
        }
    }
    ascii_chars.div_ceil(4) + non_ascii_tokens
}

/// 估算一条消息（含 reasoning 与 tool_calls）的 token 量。
pub fn estimate_message_tokens(message: &Message) -> usize {
    let mut total = MESSAGE_BASE_TOKENS + estimate_text_tokens(role_label(message.role));
    if let Some(content) = message.content.as_ref() {
        total += estimate_text_tokens(content);
    }
    if let Some(reasoning_content) = message.reasoning_content.as_ref() {
        total += estimate_text_tokens(reasoning_content);
    }
    if let Some(tool_call_id) = message.tool_call_id.as_ref() {
        total += estimate_text_tokens(tool_call_id);
    }
    if let Some(tool_calls) = message.tool_calls.as_ref() {
        total += TOOL_CALL_BASE_TOKENS
            + serde_json::to_string(tool_calls)
                .map(|value| estimate_text_tokens(&value))
                .unwrap_or_default();
    }
    total
}

/// 估算一条只有 role 与文本内容的消息的 token 量。
pub fn estimate_role_text_tokens(role: Role, content: &str) -> usize {
    MESSAGE_BASE_TOKENS + estimate_text_tokens(role_label(role)) + estimate_text_tokens(content)
}

/// 估算整个会话消息列表的 token 量（不含请求级 padding）。
pub fn estimate_conversation_tokens(conversation: &Conversation) -> usize {
    conversation
        .messages
        .iter()
        .map(estimate_message_tokens)
        .sum()
}

/// 估算工具定义序列化后的 token 量。
pub fn estimate_tool_definitions_tokens(tools: &[ToolDefinition]) -> usize {
    serde_json::to_string(tools)
        .map(|definitions| estimate_text_tokens(&definitions))
        .unwrap_or_default()
}

/// 请求级 padding：原始文本量上浮 4/3（向上取整，饱和乘法）。
pub fn apply_request_padding(tokens: usize) -> usize {
    tokens
        .saturating_mul(REQUEST_PADDING_NUMERATOR)
        .div_ceil(REQUEST_PADDING_DENOMINATOR)
}

/// 估算一次模型请求的总体 token 量：会话消息 + 工具定义，含请求级 padding。
pub fn estimate_model_request_tokens(
    conversation: &Conversation,
    tools: &[ToolDefinition],
) -> usize {
    apply_request_padding(
        estimate_conversation_tokens(conversation) + estimate_tool_definitions_tokens(tools),
    )
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::ToolCall;

    #[test]
    fn empty_text_estimates_to_zero() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn ascii_text_rounds_up_per_four_chars() {
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
    }

    #[test]
    fn non_ascii_chars_count_individually() {
        assert_eq!(estimate_text_tokens("甲乙丙"), 3);
        assert_eq!(estimate_text_tokens("ab甲乙"), 3);
    }

    #[test]
    fn message_estimate_includes_reasoning_and_tool_calls() {
        let plain = Message::assistant("answer");
        let rich = Message::assistant_tool_calls_with_content(
            "answer",
            vec![ToolCall::function("call-1", "read_file", "{}")],
        )
        .with_reasoning_content("r".repeat(400));

        let plain_tokens = estimate_message_tokens(&plain);
        let rich_tokens = estimate_message_tokens(&rich);

        assert!(rich_tokens > plain_tokens + 100);
    }

    #[test]
    fn request_estimate_grows_with_tools_and_applies_padding() {
        let conversation = Conversation::with_system_prompt("system");
        let bare = estimate_model_request_tokens(&conversation, &[]);
        let tools = vec![ToolDefinition::function(
            "large_tool",
            "x".repeat(4_000),
            serde_json::json!({"type": "object", "properties": {}}),
        )];

        let with_tools = estimate_model_request_tokens(&conversation, &tools);

        assert!(with_tools > bare + 1_000);
        // padding 4/3 向上取整
        assert_eq!(apply_request_padding(3), 4);
        assert_eq!(apply_request_padding(4), 6);
    }
}
