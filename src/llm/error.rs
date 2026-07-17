use std::error::Error as StdError;

use thiserror::Error;

/// Errors returned by LLM client configuration, request setup, or SSE parsing.
#[derive(Debug, Error)]
pub enum ChatError {
    /// The API key environment variable was not set or was empty.
    #[error("LLM_API_KEY is not set")]
    MissingApiKey,
    /// The SSE transport failed while streaming events.
    #[error("LLM SSE transport error: {0}")]
    Transport(#[source] Box<dyn StdError + Send + Sync>),
    /// The model returned a chunk that could not be decoded.
    #[error("invalid LLM response: {0}")]
    InvalidResponse(String),
    /// The model returned malformed JSON.
    #[error("failed to parse LLM response: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<reqwest::Error> for ChatError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(Box::new(error))
    }
}

impl From<sse_reqwest_client::Error> for ChatError {
    fn from(error: sse_reqwest_client::Error) -> Self {
        Self::Transport(Box::new(error))
    }
}
