//! Provider accounting envelopes must reach the editor without failing a completed turn.

mod acp_client;

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::process::Command;

use acp_client::Serve;

async fn spawn_provider_fixture()
-> Result<(String, tokio::task::JoinSet<std::io::Result<()>>), Box<dyn Error>> {
    let usage = json!({
        "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7,
        "prompt_tokens_details": {"cached_tokens": 1},
        "prompt_cache_miss_tokens": 2,
        "completion_tokens_details": {"reasoning_tokens": 2}
    });
    let choices = json!([{"delta": {"content": "ok"}, "finish_reason": "stop"}]);
    let finished = json!({"choices": choices});
    let cases = [
        vec![finished.clone(), json!({"choices": [], "usage": usage})],
        vec![json!({"choices": choices, "x_groq": {"id": "fixture", "usage": usage}})],
        vec![
            finished.clone(),
            json!({"choices": [], "usage": null, "x_groq": {"usage": usage}}),
        ],
        vec![json!({"choices": choices, "usage": usage, "x_groq": {"usage": usage}})],
    ];
    let next = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route(
            "/models",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    json!({"data": [
                        {"id": "openai/gpt-oss-120b", "context_window": 4096},
                        {"id": "deepseek-flash"},
                        {"id": "fixture-priced", "context_window": 4096}
                    ]})
                    .to_string(),
                )
            }),
        )
        .route(
            "/chat/completions",
            post(move || {
                let index = next.fetch_add(1, Ordering::SeqCst);
                let mut body = String::new();
                for chunk in cases.get(index).into_iter().flatten() {
                    body.push_str("data: ");
                    body.push_str(&chunk.to_string());
                    body.push_str("\n\n");
                }
                body.push_str("data: [DONE]\n\n");
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    Ok((base_url, server))
}

#[test_log::test(tokio::test)]
async fn deepseek_flash_usage_reports_cost_and_fallback_context() -> Result<(), Box<dyn Error>> {
    let (base_url, mut server) = spawn_provider_fixture().await?;
    let state_dir = std::env::temp_dir().join(format!("acp-flash-usage-{}", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "deepseek"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "deepseek-flash")
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("LLM_PRICING")
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd": "/tmp", "mcpServers": []})).await?;
    let prompt =
        json!({"sessionId": serve.session_id(), "prompt": [{"type": "text", "text": "hello"}]});
    let response = serve.request("session/prompt", &prompt).await?;
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("end_turn")
    );
    let updates = serve.updates("usage_update");
    let update = updates.last().ok_or("no usage update")?;
    // Fetched 2026-11-24: https://api-docs.deepseek.com/quick_start/pricing/
    assert_eq!(update.get("size").and_then(Value::as_u64), Some(1_000_000));
    assert!(
        update
            .pointer("/cost/amount")
            .and_then(Value::as_f64)
            .is_some_and(|cost| cost > 0.0)
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn override_can_price_an_unknown_model() -> Result<(), Box<dyn Error>> {
    let (base_url, mut server) = spawn_provider_fixture().await?;
    let state_dir =
        std::env::temp_dir().join(format!("acp-override-usage-{}", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "fixture-priced")
        .env(
            "LLM_PRICING",
            r#"{"fixture-priced":{"cache_hit":2,"cache_miss":2,"output":2}}"#,
        )
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd": "/tmp", "mcpServers": []})).await?;
    let prompt =
        json!({"sessionId": serve.session_id(), "prompt": [{"type": "text", "text": "hello"}]});
    serve.request("session/prompt", &prompt).await?;
    let updates = serve.updates("usage_update");
    let update = updates.last().ok_or("no usage update")?;
    assert_eq!(
        update.pointer("/cost/amount").and_then(Value::as_f64),
        Some(0.000_014)
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}

#[test]
fn invalid_pricing_warns_once_at_startup() -> Result<(), Box<dyn Error>> {
    for invalid in [
        "{secret-payload",
        r#"{"fixture":{"cache_hit":1,"unknown":2}}"#,
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(["dev", "--backend", "mock"])
            .env_remove("RUST_LOG")
            .env("LLM_PRICING", invalid)
            .output()?;
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert_eq!(stderr.matches("invalid LLM_PRICING").count(), 1, "{stderr}");
        assert!(!stderr.contains("secret-payload"));
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn usage_envelopes_reach_prompt_response_context_gauge_and_cost() -> Result<(), Box<dyn Error>>
{
    let (base_url, mut server) = spawn_provider_fixture().await?;
    let state_dir =
        std::env::temp_dir().join(format!("acp-usage-envelope-{}", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "openai/gpt-oss-120b")
        // Synthetic prices pin wiring independently of published/ambient rates.
        .env(
            "LLM_PRICING",
            r#"{"openai/gpt-oss-120b":{"cache_hit":2,"cache_miss":2,"output":2}}"#,
        )
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd": "/tmp", "mcpServers": []})).await?;
    let prompt =
        json!({"sessionId": serve.session_id(), "prompt": [{"type": "text", "text": "hello"}]});
    for index in 0..4 {
        let response = serve.request("session/prompt", &prompt).await?;
        assert_eq!(
            response
                .pointer("/result/stopReason")
                .and_then(Value::as_str),
            Some("end_turn"),
            "envelope {index}: {response}"
        );
        for (field, expected) in [
            ("totalTokens", 7),
            ("inputTokens", 3),
            ("outputTokens", 4),
            ("thoughtTokens", 2),
            ("cachedReadTokens", 1),
            ("cachedWriteTokens", 2),
        ] {
            assert_eq!(
                response
                    .pointer(&format!("/result/usage/{field}"))
                    .and_then(Value::as_u64),
                Some(expected)
            );
        }
        let updates = serve.updates("usage_update");
        assert_eq!(updates.len(), index + 1, "duplicate or lost accounting");
        let update = updates.last().ok_or("no usage update")?;
        assert_eq!(update.get("used").and_then(Value::as_u64), Some(7));
        assert_eq!(update.get("size").and_then(Value::as_u64), Some(4096));
        assert_eq!(
            update.pointer("/cost/amount").and_then(Value::as_f64),
            Some(f64::from(u32::try_from(index + 1)? * 14) / 1_000_000.0)
        );
    }
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}
