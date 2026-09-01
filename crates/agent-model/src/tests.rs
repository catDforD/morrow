use super::*;
use agent_protocol::{Message, ToolCall, ToolDefinition};
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Serves one scripted `(status, body)` response per request, in order.
/// Returns the base URL and a counter of received requests.
async fn spawn_scripted_server(
    script: Vec<(&'static str, &'static str)>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server addr");
    let requests = Arc::new(AtomicUsize::new(0));
    let counted_requests = Arc::clone(&requests);
    tokio::spawn(async move {
        for (status, body) in script {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await.expect("read request");
            counted_requests.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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
        max_retries: DEFAULT_MAX_RETRIES,
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
        max_retries: 1,
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
        max_retries: DEFAULT_MAX_RETRIES,
    })
    .expect("client")
}

async fn collect_events(mut stream: ChatCompletionStream) -> Vec<Result<ModelEvent, ModelError>> {
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
        max_retries: DEFAULT_MAX_RETRIES,
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
        max_retries: DEFAULT_MAX_RETRIES,
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
        max_retries: DEFAULT_MAX_RETRIES,
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
        max_retries: DEFAULT_MAX_RETRIES,
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
            max_retries: DEFAULT_MAX_RETRIES,
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
        max_retries: DEFAULT_MAX_RETRIES,
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
    let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"provider_specific\"}]}\n\n";
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

const RETRY_OK_BODY: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":null}]}\n\n",
    "data: [DONE]\n\n"
);

#[tokio::test]
async fn retries_transient_statuses_until_stream_establishes() {
    let (base_url, requests) = spawn_scripted_server(vec![
        ("500 Internal Server Error", "boom"),
        ("500 Internal Server Error", "boom"),
        ("200 OK", RETRY_OK_BODY),
    ])
    .await;
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: DEFAULT_MAX_RETRIES,
    })
    .expect("client");

    let stream = client
        .stream_chat(&conversation(), &[])
        .await
        .expect("stream chat");
    let events = collect_events(stream).await;

    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert!(matches!(
        events.as_slice(),
        [Ok(ModelEvent::TextDelta(text)), Ok(ModelEvent::Completed)] if text == "recovered"
    ));
}

#[tokio::test]
async fn non_retryable_status_fails_immediately_without_retrying() {
    let (base_url, requests) = spawn_scripted_server(vec![("400 Bad Request", "nope")]).await;
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: DEFAULT_MAX_RETRIES,
    })
    .expect("client");

    let err = match client.stream_chat(&conversation(), &[]).await {
        Ok(_) => panic!("stream_chat must fail"),
        Err(err) => err,
    };

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(matches!(err, ModelError::HttpStatus { status: 400, .. }));
}

#[tokio::test]
async fn persistent_retryable_status_fails_after_max_attempts() {
    let (base_url, requests) = spawn_scripted_server(vec![
        ("429 Too Many Requests", "slow down"),
        ("429 Too Many Requests", "slow down"),
        ("429 Too Many Requests", "slow down"),
        ("429 Too Many Requests", "slow down"),
    ])
    .await;
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: DEFAULT_MAX_RETRIES,
    })
    .expect("client");

    let err = match client.stream_chat(&conversation(), &[]).await {
        Ok(_) => panic!("stream_chat must fail"),
        Err(err) => err,
    };

    assert_eq!(requests.load(Ordering::SeqCst), 3);
    let message = err.to_string();
    assert!(message.contains("429"), "unexpected error: {message}");
    assert!(
        message.contains("after 3 attempts"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn zero_max_retries_disables_retrying() {
    let (base_url, requests) = spawn_scripted_server(vec![
        ("500 Internal Server Error", "boom"),
        ("200 OK", RETRY_OK_BODY),
    ])
    .await;
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: 0,
    })
    .expect("client");

    let err = match client.stream_chat(&conversation(), &[]).await {
        Ok(_) => panic!("stream_chat must fail"),
        Err(err) => err,
    };

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(matches!(err, ModelError::HttpStatus { status: 500, .. }));
}

#[tokio::test]
async fn connect_errors_are_retried_and_report_attempts() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unused address");
    let addr = listener.local_addr().expect("unused address");
    drop(listener);
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url: format!("http://{addr}/v1"),
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(1),
        max_retries: 2,
    })
    .expect("client");

    let err = match client.stream_chat(&conversation(), &[]).await {
        Ok(_) => panic!("closed address must fail"),
        Err(err) => err,
    };

    let message = err.to_string();
    assert!(
        message.contains("failed to send model request"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("after 2 attempts"),
        "unexpected error: {message}"
    );
}

#[test]
fn retry_backoff_doubles_and_caps_at_max() {
    assert_eq!(retry_backoff(1), Duration::from_millis(500));
    assert_eq!(retry_backoff(2), Duration::from_secs(1));
    assert_eq!(retry_backoff(3), Duration::from_secs(2));
    assert_eq!(retry_backoff(4), Duration::from_secs(4));
    assert_eq!(retry_backoff(5), Duration::from_secs(8));
    assert_eq!(retry_backoff(20), Duration::from_secs(8));
}
