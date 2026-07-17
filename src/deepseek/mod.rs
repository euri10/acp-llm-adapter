//! `DeepSeek` client primitives and streaming SSE adapter.

mod client;
mod config;
mod error;
mod stream;
mod types;

pub use client::{DeepSeekClient, LlmClient, fetch_available_models};
pub use config::DeepSeekConfig;
pub use error::DeepSeekError;
pub use types::{
    ChatMessage, ChatRequest, FinishReason, MessageRole, StreamEvent, ToolCall, ToolCallDelta,
    ToolDefinition, UsageData,
};

#[cfg(test)]
mod tests;
