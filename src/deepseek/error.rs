use thiserror::Error;

/// Errors returned by `DeepSeek` configuration, request setup, or SSE parsing.
#[derive(Debug, Error)]
pub enum DeepSeekError {
    /// The `DEEPSEEK_API_KEY` environment variable was not set or was empty.
    #[error("DEEPSEEK_API_KEY is not set")]
    MissingApiKey,
    /// The request could not be cloned for SSE streaming.
    #[error("failed to clone DeepSeek streaming request: {0}")]
    RequestClone(#[from] reqwest_eventsource::CannotCloneRequestError),
    /// The SSE transport failed while streaming events.
    #[error("`DeepSeek` SSE transport error: {0}")]
    Transport(Box<reqwest_eventsource::Error>),
    /// The model returned a chunk that could not be decoded.
    #[error("invalid DeepSeek response: {0}")]
    InvalidResponse(String),
    /// The model returned malformed JSON.
    #[error("failed to parse DeepSeek response: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<reqwest_eventsource::Error> for DeepSeekError {
    fn from(error: reqwest_eventsource::Error) -> Self {
        Self::Transport(Box::new(error))
    }
}
