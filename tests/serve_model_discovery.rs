//! The startup model fetch must target the same endpoint the chat client does.
//!
//! `LLM_BASE_URL` is documented as overriding the provider's base URL. When
//! only the chat client honoured it, a session sent completions to the override
//! and `GET /models` to the backend's compiled-in default, carrying the
//! configured API key to a host the operator never named (daa-base-url-desync-sx0r).

use std::error::Error;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// A stand-in provider that records whether its `/models` route was called.
async fn spawn_provider_fixture()
-> Result<(String, Arc<AtomicBool>, CancellationToken), Box<dyn Error>> {
    let hit = Arc::new(AtomicBool::new(false));
    let cancellation = CancellationToken::new();

    let router = Router::new()
        .route(
            "/models",
            get(|State(hit): State<Arc<AtomicBool>>| async move {
                hit.store(true, Ordering::SeqCst);
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"data":[{"id":"sentinel-model","context_window":123456}]}"#,
                )
            }),
        )
        .route("/chat/completions", post(|| async {
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
             "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\ndata: [DONE]\n\n")
        }))
        .with_state(Arc::clone(&hit));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancellation.cancelled_owned().await })
                .await;
        }
    });

    Ok((format!("http://{address}"), hit, cancellation))
}

/// Run one `initialize` round-trip against `serve`, with the given environment.
async fn initialize_once(base_url: &str) -> Result<(), Box<dyn Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "sentinel-model")
        .env_remove("ACP_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
    let stdout = child.stdout.take().ok_or("serve exposed no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            }
        }
    });
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(Box::new)?;

    // The fetch happens during startup, before the first response is written.
    while let Some(line) = lines.next_line().await? {
        if line.contains("\"id\":1") {
            break;
        }
    }

    // Exercise startup discovery -> session state -> real usage notification.
    let request = json!({"jsonrpc":"2.0", "id":2, "method":"session/new",
        "params":{"cwd":"/tmp", "mcpServers":[]}});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    let session_id = loop {
        let line = lines.next_line().await?.ok_or("serve closed stdout")?;
        let message: serde_json::Value = serde_json::from_str(&line)?;
        if message.get("id").and_then(serde_json::Value::as_u64) == Some(2) {
            break message
                .pointer("/result/sessionId")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing session id")?
                .to_string();
        }
    };
    let request = json!({"jsonrpc":"2.0", "id":3, "method":"session/prompt",
        "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    let mut sizes = Vec::new();
    loop {
        let line = lines.next_line().await?.ok_or("serve closed stdout")?;
        let message: serde_json::Value = serde_json::from_str(&line)?;
        if message
            .pointer("/params/update/sessionUpdate")
            .and_then(serde_json::Value::as_str)
            == Some("usage_update")
        {
            sizes.push(
                message
                    .pointer("/params/update/size")
                    .and_then(serde_json::Value::as_u64),
            );
        }
        if message.get("id").and_then(serde_json::Value::as_u64) == Some(3) {
            assert_eq!(
                message
                    .pointer("/result/stopReason")
                    .and_then(serde_json::Value::as_str),
                Some("end_turn")
            );
            break;
        }
    }

    child.kill().await?;
    assert_eq!(sizes, vec![Some(123_456)]);
    Ok(())
}

#[test_log::test(tokio::test)]
async fn startup_model_fetch_targets_the_configured_base_url() -> Result<(), Box<dyn Error>> {
    let (base_url, hit, cancellation) = spawn_provider_fixture().await?;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        initialize_once(&base_url),
    )
    .await;
    let reached_override = hit.load(Ordering::SeqCst);

    cancellation.cancel();
    result??;

    assert!(
        reached_override,
        "LLM_BASE_URL was set to the fixture, but it received no /models request, \
         so the model fetch went to the backend's compiled-in default instead"
    );
    Ok(())
}
