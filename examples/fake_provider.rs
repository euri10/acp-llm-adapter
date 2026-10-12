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
//! without a key or a network. It serves `GET /models` and answers
//! `POST /chat/completions` with a streamed reply chosen by the conversation:
//!
//! - a last user message `!tool <name> <json-arguments>` gets that tool call,
//!   so editor checks can drive permission prompts and edit previews;
//! - a last tool result gets `tool said: <result>` on one line;
//! - anything else gets the fixed reply.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const DEFAULT_REPLY: &str = "Hello from acp-llm-adapter.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let reply = std::env::var("FAKE_PROVIDER_REPLY").unwrap_or_else(|_| DEFAULT_REPLY.into());
    let key = std::env::var("FAKE_PROVIDER_KEY")?;
    let expected = format!("Bearer {key}");
    let calls = Arc::new(AtomicUsize::new(0));
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
            post(move |headers: HeaderMap, request: String| async move {
                let authorized = headers
                    .get(AUTHORIZATION)
                    .is_some_and(|value| value.as_bytes() == expected.as_bytes());
                if authorized {
                    eprintln!("completion authorized");
                    let body = respond(&request, &reply, &calls);
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

/// Choose the streamed reply from the last message of the chat request.
fn respond(request: &str, reply: &str, calls: &AtomicUsize) -> String {
    let last = serde_json::from_str::<Value>(request)
        .ok()
        .and_then(|request| request.get("messages")?.as_array()?.last().cloned())
        .unwrap_or(Value::Null);
    let content = last.get("content").and_then(Value::as_str).unwrap_or("");
    if last.get("role").and_then(Value::as_str) == Some("tool") {
        let result = content.split_whitespace().collect::<Vec<_>>().join(" ");
        return text_body(&format!("tool said: {result}"));
    }
    if let Some((name, arguments)) = content
        .strip_prefix("!tool ")
        .and_then(|directive| directive.split_once(' '))
    {
        let id = format!("fake-call-{}", calls.fetch_add(1, Ordering::SeqCst));
        return tool_call_body(&id, name, arguments.trim());
    }
    text_body(reply)
}

/// One SSE response: the reply split into word deltas, a stop, then usage.
fn text_body(reply: &str) -> String {
    let mut chunks: Vec<_> = reply
        .split_inclusive(' ')
        .map(|word| json!({"choices":[{"index":0,"delta":{"content":word},"finish_reason":null}]}))
        .collect();
    chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    sse(chunks)
}

/// One SSE response asking for a single tool call, as a provider streams it.
fn tool_call_body(id: &str, name: &str, arguments: &str) -> String {
    sse(vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,
            "type":"function","function":{"name":name,"arguments":""}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":arguments}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

fn sse(mut chunks: Vec<Value>) -> String {
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
