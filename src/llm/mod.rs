//! LLM client primitives and streaming SSE adapter.

mod client;
mod config;
mod error;
mod stream;
mod types;

pub use client::{ChatClient, LlmClient, fetch_available_models};
pub use config::ChatConfig;
pub use error::ChatError;
pub use types::{
    ChatMessage, ChatRequest, FinishReason, MessageRole, StreamEvent, ToolCall, ToolCallDelta,
    ToolDefinition, UsageData,
};

#[cfg(test)]
mod tests;
