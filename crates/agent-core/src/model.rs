use super::*;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub conversation: Conversation,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCalls(Vec<ToolCall>),
    Completed,
}

type BoxError = Box<dyn StdError + Send + Sync + 'static>;

#[derive(Debug)]
pub struct ModelFailure {
    source: BoxError,
}

impl ModelFailure {
    pub fn new(error: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }
}

impl fmt::Display for ModelFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl StdError for ModelFailure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

pub type ModelStream = BoxStream<'static, Result<ModelEvent, ModelFailure>>;
pub type ModelFuture = BoxFuture<'static, Result<ModelStream, ModelFailure>>;

pub trait Model: Send + Sync {
    /// 返回拥有所有数据的 future，便于 turn 状态机跨多次 poll 持有模型请求。
    fn stream(&self, request: ModelRequest) -> ModelFuture;

    /// 返回可安全共享给隔离子任务的模型副本。不支持共享的实现保持默认 `None`。
    fn shared_clone(&self) -> Option<Shared<dyn Model>> {
        None
    }
}
