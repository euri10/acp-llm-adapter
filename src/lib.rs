#![forbid(unsafe_code)]
#![deny(
    warnings,
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

//! `DeepSeek` client support for the `ACP` adapter.
//!
//! The adapter proper still needs the ACP session layer, but the DeepSeek-side
//! seam lives here so it can be tested in isolation and reused by the later
//! protocol wiring.
//!
//! # Overview
//!
//! The [`deepseek`] module exposes:
//! - request primitives such as [`deepseek::ChatMessage`] and [`deepseek::ChatRequest`]
//! - tool advertisement types such as [`deepseek::ToolDefinition`]
//! - streamed response events via [`deepseek::StreamEvent`]
//! - an HTTP-backed client via [`deepseek::DeepSeekClient`]
//!
//! # Examples
//!
//! Create a simple streaming request:
//!
//! ```rust,no_run
//! use deepseek_acp_adapter::deepseek::{ChatMessage, ChatRequest, DeepSeekClient, LlmClient};
//! use futures_util::StreamExt;
//! use tokio_util::sync::CancellationToken;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = DeepSeekClient::from_env()?;
//!     let request = ChatRequest::new(vec![
//!         ChatMessage::system("You are a concise coding assistant."),
//!         ChatMessage::user("Explain what this adapter crate does."),
//!     ]);
//!
//!     let mut stream = client.stream_chat(request, CancellationToken::new())?;
//!     while let Some(event) = stream.next().await {
//!         println!("{:?}", event?);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! Build a tool-enabled request:
//!
//! ```rust
//! use deepseek_acp_adapter::deepseek::{ChatMessage, ChatRequest, ToolDefinition};
//!
//! let request = ChatRequest::new(vec![ChatMessage::user("Read src/lib.rs")]).with_tools(vec![
//!     ToolDefinition::new(
//!         "read_file",
//!         "Read a UTF-8 text file",
//!         serde_json::json!({
//!             "type": "object",
//!             "properties": {
//!                 "path": { "type": "string" }
//!             },
//!             "required": ["path"],
//!             "additionalProperties": false
//!         }),
//!     ),
//! ]);
//!
//! assert_eq!(request.tools()[0].name(), "read_file");
//! ```

/// `DeepSeek` client primitives and streaming SSE adapter.
pub mod deepseek;
