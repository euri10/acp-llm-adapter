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
use axum::routing::get;
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
                    r#"{"data":[{"id":"sentinel-model"}]}"#,
                )
            }),
        )
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
        .env_remove("LLM_MODEL")
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

    child.kill().await?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn startup_model_fetch_targets_the_configured_base_url() -> Result<(), Box<dyn Error>> {
    let (base_url, hit, cancellation) = spawn_provider_fixture().await?;

    initialize_once(&base_url).await?;
    let reached_override = hit.load(Ordering::SeqCst);

    cancellation.cancel();

    assert!(
        reached_override,
        "LLM_BASE_URL was set to the fixture, but it received no /models request, \
         so the model fetch went to the backend's compiled-in default instead"
    );
    Ok(())
}
