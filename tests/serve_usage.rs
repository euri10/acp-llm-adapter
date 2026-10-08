//! Invalid provider counters must fail without poisoning a live ACP session.

mod acp_client;

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::process::Command;

use acp_client::Serve;

async fn spawn_provider_fixture(
    cases: Vec<Value>,
) -> Result<(String, tokio::task::JoinSet<std::io::Result<()>>), Box<dyn Error>> {
    let router = Router::new()
        .route("/models", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({"data": [{"id": "deepseek-v4-pro", "context_window": 4096}]}).to_string())
        }))
        .route("/chat/completions", post(move |State(next): State<Arc<AtomicUsize>>| {
            let index = next.fetch_add(1, Ordering::SeqCst);
            let usage = if index.is_multiple_of(2) {
                cases.get(index / 2).cloned().unwrap_or(Value::Null)
            } else {
                json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7, "context_length": 4096})
            };
            async move {
                let payload = json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}], "usage": usage});
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    format!("data: {payload}\n\ndata: [DONE]\n\n"))
            }
        }))
        .with_state(Arc::new(AtomicUsize::new(0)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    Ok((base_url, server))
}

#[test_log::test(tokio::test)]
async fn serve_rejects_usage_overflow_and_recovers() -> Result<(), Box<dyn Error>> {
    let invalid_usage = vec![
        json!({"prompt_tokens": u64::MAX, "completion_tokens": 1, "context_length": 4096}),
        json!({"prompt_tokens": u64::MAX, "completion_tokens": 1, "total_tokens": u64::MAX, "context_length": 4096}),
        // The token total fits, but its cost does not at the synthetic $2/M rate.
        json!({"prompt_tokens": u64::MAX, "completion_tokens": 0, "context_length": 4096}),
        // This cost fits alone but exceeds the session total after recovery prompts.
        json!({"prompt_tokens": u64::MAX / 2, "completion_tokens": 0, "context_length": 4096}),
    ];
    let invalid_count = invalid_usage.len();
    let (base_url, mut server) = spawn_provider_fixture(invalid_usage).await?;
    let state_dir = std::env::temp_dir().join(format!("acp-usage-test-{}", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "deepseek-v4-pro")
        // Synthetic fixture prices, independent of published or ambient rates.
        .env(
            "LLM_PRICING",
            r#"{"deepseek-v4-pro":{"cache_hit":2,"cache_miss":2,"output":2}}"#,
        )
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd": "/tmp", "mcpServers": []})).await?;
    let prompt =
        json!({"sessionId": serve.session_id(), "prompt": [{"type": "text", "text": "hello"}]});

    for index in 0..invalid_count {
        let failed = serve
            .request("session/prompt", &prompt)
            .await
            .map_err(|error| {
                format!("session/prompt with invalid usage (case {index}): {error}")
            })?;
        assert_eq!(
            failed.pointer("/error/code").and_then(Value::as_i64),
            Some(-32603)
        );
        assert_eq!(
            failed.pointer("/error/data").and_then(Value::as_str),
            Some("provider returned an invalid response")
        );
        assert!(failed.get("result").is_none());
        assert_eq!(
            serve.updates("usage_update").len(),
            index,
            "invalid usage was emitted"
        );

        let recovered = serve
            .request("session/prompt", &prompt)
            .await
            .map_err(|error| format!("session/prompt recovery (case {index}): {error}"))?;
        assert_eq!(
            recovered
                .pointer("/result/stopReason")
                .and_then(Value::as_str),
            Some("end_turn")
        );
        assert_eq!(
            recovered
                .pointer("/result/usage/totalTokens")
                .and_then(Value::as_u64),
            Some(7)
        );
        let updates = serve.updates("usage_update");
        let update = updates.last().ok_or("recovery emitted no usage update")?;
        assert_eq!(updates.len(), index + 1);
        assert_eq!(update.get("used").and_then(Value::as_u64), Some(7));
        assert_eq!(update.get("size").and_then(Value::as_u64), Some(4096));
        let expected_micros = u32::try_from(index + 1)? * 14;
        assert_eq!(
            update.pointer("/cost/amount").and_then(Value::as_f64),
            Some(f64::from(expected_micros) / 1_000_000.0)
        );
    }

    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}
