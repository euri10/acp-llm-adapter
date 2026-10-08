//! Private provider and configuration details must not cross the ACP boundary.

use std::error::Error;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

const SENTINEL: &str = "AUDIT_PROVIDER_PRIVATE_SENTINEL";

async fn request(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn Error>> {
    let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| format!("serve closed stdout during {method} (request {id})"))?;
            let response: Value = serde_json::from_str(&line)?;
            assert!(
                !line.contains(SENTINEL),
                "private data reached the editor during {method}"
            );
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(response);
            }
        }
    })
    .await?
}

fn assert_error(response: &Value, code: i64, message: &str) {
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(code)
    );
    assert_eq!(
        response.pointer("/error/data").and_then(Value::as_str),
        Some(message)
    );
}

async fn check_validation_errors(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    session_id: &str,
) -> Result<(), Box<dyn Error>> {
    for (index, (method, params, message)) in [
        ("session/prompt", json!({"sessionId":SENTINEL, "prompt":[{"type":"text", "text":"hello"}]}), "unknown session id"),
        ("session/set_mode", json!({"sessionId":session_id, "modeId":SENTINEL}), "unsupported session mode"),
        ("session/set_config_option", json!({"sessionId":session_id, "configId":SENTINEL, "value":"bad"}), "unsupported session config option"),
        ("session/set_config_option", json!({"sessionId":session_id, "configId":"model", "value":SENTINEL}), "unsupported model"),
        ("session/set_config_option", json!({"sessionId":session_id, "configId":"reasoning_effort", "value":SENTINEL}), "unsupported reasoning effort"),
        ("session/set_config_option", json!({"sessionId":session_id, "configId":"max_tokens", "value":SENTINEL}), "unsupported max_tokens value"),
        ("session/close", json!({"sessionId":SENTINEL}), "unknown session id"),
        ("session/delete", json!({"sessionId":SENTINEL}), "unknown session id"),
        ("session/load", json!({"sessionId":SENTINEL, "cwd":SENTINEL, "mcpServers":[]}), "cwd must be absolute"),
        ("session/new", json!({"cwd":"/tmp", "mcpServers":[{"type":"stdio", "name":SENTINEL, "command":SENTINEL, "args":[], "env":[]}]}), "MCP server command must be absolute"),
        ("session/new", json!({"cwd":"/tmp", "mcpServers":[{"type":"http", "name":SENTINEL, "url":"http://127.0.0.1:1", "headers":[{"name":format!("bad\n{SENTINEL}"), "value":"value"}]}]}), "invalid HTTP header name"),
        ("session/new", json!({"cwd":"/tmp", "mcpServers":[{"type":"http", "name":SENTINEL, "url":"http://127.0.0.1:1", "headers":[{"name":"Authorization", "value":format!("Bearer {SENTINEL}\n")}]}]}), "invalid HTTP header value"),
        ("session/new", json!({"cwd":"/tmp", "mcpServers":[{"type":"http", "name":SENTINEL, "url":format!("://{SENTINEL}"), "headers":[]}]}), "failed to initialize MCP server"),
    ].into_iter().enumerate() {
        let response = request(stdin, lines, 10 + u64::try_from(index)?, method, params).await?;
        assert_error(&response, -32602, message);
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn serve_sanitizes_provider_and_validation_errors() -> Result<(), Box<dyn Error>> {
    let first = Arc::new(AtomicBool::new(true));
    let router = Router::new()
        .route("/models", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({"data":[{"id":"fixture-model", "context_window":8192}]}).to_string())
        }))
        .route("/chat/completions", post(|State(first): State<Arc<AtomicBool>>| async move {
            let usage = if first.swap(false, Ordering::SeqCst) {
                json!({"prompt_tokens":SENTINEL})
            } else {
                json!({"prompt_tokens":3, "completion_tokens":4, "total_tokens":7, "future_field":SENTINEL})
            };
            let payload = json!({"choices":[{"delta":{"content":"ok"}, "finish_reason":"stop"}], "usage":usage});
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {payload}\n\ndata: [DONE]\n\n"))
        }))
        .with_state(first);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    // JoinSet owns and aborts the fixture task even if an assertion fails.
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    let state_dir = std::env::temp_dir().join(format!("acp-error-test-{}", uuid::Uuid::new_v4()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "fixture-model")
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("ACP_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
    let mut lines = BufReader::new(child.stdout.take().ok_or("serve exposed no stdout")?).lines();
    let initialized = request(&mut stdin, &mut lines, 1, "initialize", json!({
        "protocolVersion":1,
        "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false}, "terminal":false}
    })).await?;
    assert!(initialized.get("result").is_some(), "initialize failed");
    let session = request(
        &mut stdin,
        &mut lines,
        2,
        "session/new",
        json!({"cwd":"/tmp", "mcpServers":[]}),
    )
    .await?;
    let session_id = session
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("session/new returned no session id")?;
    let prompt = json!({"sessionId":session_id, "prompt":[{"type":"text", "text":"hello"}]});
    let failed = request(&mut stdin, &mut lines, 3, "session/prompt", prompt.clone()).await?;
    assert_error(&failed, -32603, "provider returned an invalid response");
    // The same session must remain usable after the provider error.
    let recovered = request(&mut stdin, &mut lines, 4, "session/prompt", prompt).await?;
    assert_eq!(
        recovered
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("end_turn")
    );
    check_validation_errors(&mut stdin, &mut lines, session_id).await?;
    let after_invalid_setup = request(
        &mut stdin,
        &mut lines,
        30,
        "session/new",
        json!({"cwd":"/tmp", "mcpServers":[]}),
    )
    .await?;
    assert!(
        after_invalid_setup
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .is_some(),
        "serve must still accept sessions after invalid MCP setup"
    );
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await??
            .success()
    );
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}
