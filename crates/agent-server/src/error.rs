use super::*;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    ModelSettings(#[from] ModelRegistryError),
    #[error("failed to bind server at {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("server failed: {0}")]
    Serve(#[source] std::io::Error),
    #[error("server task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
    #[error("server has {0} running turn(s)")]
    RunningTurns(usize),
    #[error("failed to generate a browser session token: {0}")]
    Random(String),
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
            })),
        )
            .into_response()
    }
}

impl From<agent_runtime::RuntimeError> for ApiError {
    fn from(error: agent_runtime::RuntimeError) -> Self {
        Self::internal(error.to_string())
    }
}
