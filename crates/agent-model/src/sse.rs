use super::*;

pub struct ChatCompletionStream {
    inner: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    buffer: Vec<u8>,
    pending: VecDeque<Result<ModelEvent, ModelError>>,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    done: bool,
    saw_text: bool,
}

impl ChatCompletionStream {
    pub(crate) fn new(inner: BoxStream<'static, Result<Bytes, reqwest::Error>>) -> Self {
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
