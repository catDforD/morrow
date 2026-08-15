use agent_core::{Model, ModelEvent, ModelFailure, ModelFuture, ModelRequest, ModelStream};
use agent_protocol::{Message, ToolCall};
use futures_util::future::FutureExt;
use futures_util::stream::{self, StreamExt};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Error surfaced when the scenario script runs out of model responses.
/// This is always a scenario bug or an agent regression (more model calls
/// than the scenario expected), never silent.
#[derive(Debug, Error)]
#[error("scripted model exhausted: model call {} requested but script has only {} response(s)", .model_call_index, .script_len)]
pub struct ScriptedModelExhausted {
    pub model_call_index: usize,
    pub script_len: usize,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ScriptedModelError(String);

/// A snapshot of one `ModelRequest` received by the scripted model.
#[derive(Debug, Clone)]
pub struct RecordedModelRequest {
    pub messages: Vec<Message>,
    pub tool_definitions: Vec<agent_protocol::ToolDefinition>,
}

/// One model response: the ordered events one stream call yields.
pub type ScriptedResponse = Vec<Result<ModelEvent, ModelFailure>>;

/// A deterministic `Model` that replays a fixed script and records every
/// request. There is no network, no sampling and no wall-clock dependency:
/// the same scenario run twice produces the same events and metrics.
pub struct ScriptedModel {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<RecordedModelRequest>>>,
}

impl ScriptedModel {
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a scripted model from validated scenario events. String errors
    /// are wrapped as model failures.
    pub fn from_scenario(events: Vec<Vec<Result<ModelEvent, String>>>) -> Self {
        let responses = events
            .into_iter()
            .map(|response| {
                response
                    .into_iter()
                    .map(|event| {
                        event.map_err(|message| ModelFailure::new(ScriptedModelError(message)))
                    })
                    .collect()
            })
            .collect();
        Self::new(responses)
    }

    pub fn record_requests(&self) -> Arc<Mutex<Vec<RecordedModelRequest>>> {
        Arc::clone(&self.requests)
    }
}

impl Model for ScriptedModel {
    fn stream(&self, request: ModelRequest) -> ModelFuture {
        let script_len = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let model_call_index = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedModelRequest {
                messages: request.conversation.messages.clone(),
                tool_definitions: request.tools.clone(),
            });

        let response = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();

        async move {
            let stream: ModelStream = match response {
                Some(events) => stream::iter(events).boxed(),
                None => stream::once(async move {
                    Err(ModelFailure::new(ScriptedModelExhausted {
                        model_call_index,
                        script_len,
                    }))
                })
                .boxed(),
            };
            Ok(stream)
        }
        .boxed()
    }
}

/// Convenience for scripts: an explicit tool call with a valid JSON-ish
/// argument payload.
pub fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::function(id, name, arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn scripted_model_records_requests_and_replays_in_order() {
        let model = ScriptedModel::from_scenario(vec![
            vec![Ok(ModelEvent::TextDelta("first".to_string()))],
            vec![Ok(ModelEvent::Completed)],
        ]);

        let mut first = model
            .stream(ModelRequest {
                conversation: agent_protocol::Conversation::with_system_prompt("system"),
                tools: Vec::new(),
            })
            .await
            .expect("first stream");
        let mut second = model
            .stream(ModelRequest {
                conversation: agent_protocol::Conversation::with_system_prompt("system"),
                tools: Vec::new(),
            })
            .await
            .expect("second stream");

        assert_eq!(
            first.next().await.unwrap().unwrap(),
            ModelEvent::TextDelta("first".to_string())
        );
        assert!(first.next().await.is_none());
        assert_eq!(second.next().await.unwrap().unwrap(), ModelEvent::Completed);
        assert_eq!(model.record_requests().lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn exhausted_script_reports_a_model_failure() {
        let model = ScriptedModel::from_scenario(Vec::new());
        let mut stream = model
            .stream(ModelRequest {
                conversation: agent_protocol::Conversation::new(),
                tools: Vec::new(),
            })
            .await
            .expect("stream");

        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(error.contains("scripted model exhausted"), "{error}");
    }
}
