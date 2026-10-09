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

//! Offline OpenAI-compatible provider for the editor snippet checks.
//!
//! `scripts/test-editor-snippets` points a snippet's real backend at this
//! server through `LLM_BASE_URL`, so the README configuration runs unchanged
//! without a key or a network. It serves `GET /models` and answers every
//! `POST /chat/completions` with a fixed streamed reply.
//!
//! Completions require `Authorization: Bearer $FAKE_PROVIDER_KEY` and get 401
//! otherwise, so a snippet that fails to pass its key cannot pass the check.
//! Each completion logs `completion authorized` or `completion unauthorized`
//! to stderr, for checks that cannot read the editor's buffer.
//!
//! It binds `127.0.0.1` on an ephemeral port, prints its base URL as the first
//! stdout line, and exits when stdin reaches EOF, so the process that owns the
//! pipe also owns its lifetime. Set `FAKE_PROVIDER_REPLY` to change the reply.

use std::error::Error;
use std::io::Write;

use axum::Router;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::json;
use tokio::io::AsyncReadExt;

const DEFAULT_REPLY: &str = "Hello from acp-llm-adapter.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let reply = std::env::var("FAKE_PROVIDER_REPLY").unwrap_or_else(|_| DEFAULT_REPLY.into());
    let key = std::env::var("FAKE_PROVIDER_KEY")?;
    let expected = format!("Bearer {key}");
    let body = completion_body(&reply);
    let app = Router::new()
        .route(
            "/models",
            get(|| async {
                (
                    [(CONTENT_TYPE, "application/json")],
                    json!({"data":[{"id":"deepseek-v4-pro"}]}).to_string(),
                )
            }),
        )
        .route(
            "/chat/completions",
            post(move |headers: HeaderMap| async move {
                let authorized = headers
                    .get(AUTHORIZATION)
                    .is_some_and(|value| value.as_bytes() == expected.as_bytes());
                if authorized {
                    eprintln!("completion authorized");
                    ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
                } else {
                    eprintln!("completion unauthorized");
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "http://{}", listener.local_addr()?)?;
    stdout.flush()?;

    tokio::select! {
        served = axum::serve(listener, app) => served?,
        () = stdin_closed() => {}
    }
    Ok(())
}

/// One SSE response: the reply split into word deltas, a stop, then usage.
fn completion_body(reply: &str) -> String {
    let mut chunks: Vec<_> = reply
        .split_inclusive(' ')
        .map(|word| json!({"choices":[{"index":0,"delta":{"content":word},"finish_reason":null}]}))
        .collect();
    chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    chunks.push(json!({"choices":[],"usage":{
        "prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}));
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

async fn stdin_closed() {
    let mut stdin = tokio::io::stdin();
    let mut buffer = [0_u8; 64];
    while matches!(stdin.read(&mut buffer).await, Ok(read) if read > 0) {}
}
