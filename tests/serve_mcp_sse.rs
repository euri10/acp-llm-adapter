//! Legacy MCP HTTP+SSE through the shipped stdio adapter and a real local server.

mod acp_client;
mod mcp_sse_fixture;

use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use acp_client::{Serve, Stopped};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use mcp_sse_fixture::{Behavior, LegacyServer, TOKEN};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(8);
const TOOL: &str = "mcp__legacy__echo";

struct Fixture {
    root: PathBuf,
    base_url: String,
    requests: UnboundedReceiver<Value>,
    provider: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!("mcp-sse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let (sender, requests) = unbounded_channel();
        let router = axum::Router::new()
            .route("/models", get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                    json!({"data": [{"id": "fixture-model"}]}).to_string())
            }))
            .route("/chat/completions", post(move |body: String| {
                let sender = sender.clone();
                async move {
                    let Ok(request) = serde_json::from_str::<Value>(&body) else {
                        return (axum::http::StatusCode::BAD_REQUEST, "invalid fixture request").into_response();
                    };
                    let last = request.get("messages").and_then(Value::as_array).and_then(|messages| messages.last());
                    let user = last.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user");
                    let hold = last.and_then(|m| m.get("content")).and_then(Value::as_str) == Some("hold");
                    let delta = if user {
                        json!({"tool_calls": [{"index": 0, "id": "legacy-call", "type": "function",
                            "function": {"name": TOOL, "arguments": json!({"message": if hold {"hold"} else {"sentinel"}}).to_string()}}]})
                    } else { json!({"content": "done"}) };
                    // This test owns the receiver; no response evidence is needed after it drops.
                    let _ = sender.send(request);
                    let frame = json!({"choices": [{"index": 0, "delta": delta,
                        "finish_reason": if user {"tool_calls"} else {"stop"}}]});
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        format!("data: {frame}\n\ndata: [DONE]\n\n")).into_response()
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let provider = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "local SSE provider fixture failed");
            }
        });
        Ok(Self {
            root,
            base_url,
            requests,
            provider,
        })
    }

    async fn start(&self, servers: Value) -> Result<Serve, Box<dyn Error>> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        command
            .args(["serve", "--backend", "groq"])
            .env("LLM_API_KEY", "fixture-key")
            .env("LLM_BASE_URL", &self.base_url)
            .env("LLM_MODEL", "fixture-model")
            .env("XDG_STATE_HOME", &self.root)
            .env_remove("ACP_LOG");
        Serve::start_with(command, json!({"cwd": self.root, "mcpServers": servers})).await
    }

    async fn request(&mut self) -> Result<Value, Box<dyn Error>> {
        tokio::time::timeout(LIMIT, self.requests.recv())
            .await?
            .ok_or_else(|| "provider fixture stopped".into())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.provider.abort();
        // The fixture uniquely allocated this directory and owns every artifact.
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            tracing::error!(%error, "could not remove SSE test state");
        }
    }
}

fn servers(server: &LegacyServer) -> Value {
    json!([{"type": "sse", "name": "legacy", "url": server.url,
        "headers": [{"name": "Authorization", "value": TOKEN}]}])
}

async fn complete(
    serve: &mut Serve,
    id: u64,
    option: Option<&str>,
    reason: &str,
) -> Result<(), Box<dyn Error>> {
    let outcome = serve
        .pump_with_permission(LIMIT, Some(id), || false, option)
        .await?;
    let Stopped::Response(response) = outcome else {
        return Err(format!("prompt did not finish: {outcome:?}").into());
    };
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some(reason),
        "{response}"
    );
    Ok(())
}

async fn wait_closed(server: &LegacyServer) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(LIMIT, async {
        while server.observed.active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(
        server.observed.gets.load(Ordering::SeqCst),
        1,
        "legacy session was replayed"
    );
    Ok(())
}

#[test_log::test(tokio::test)]
async fn legacy_sse_discovery_approval_denial_and_pending_cancellation()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let server = LegacyServer::start(None).await?;
    let mut serve = fixture.start(servers(&server)).await?;
    assert_eq!(server.observed.gets.load(Ordering::SeqCst), 1);
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("reject_once"), "end_turn").await?;
    let request = fixture.request().await?;
    assert!(
        request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools
                .iter()
                .any(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(TOOL)))
    );
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("rejected by permission policy")
    );
    assert_eq!(server.observed.calls.load(Ordering::SeqCst), 0);

    let id = serve.start_prompt("echo").await?;
    let pending = serve
        .pump_with_permission(LIMIT, Some(id), || false, None)
        .await?;
    let Stopped::Permission(permission) = pending else {
        return Err(format!("no approval: {pending:?}").into());
    };
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    complete(&mut serve, id, None, "cancelled").await?;
    serve
        .select_permission(
            permission.get("id").ok_or("permission id missing")?,
            "allow_once",
        )
        .await?;
    assert_eq!(server.observed.calls.load(Ordering::SeqCst), 0);
    fixture.request().await?;

    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("allow_once"), "end_turn").await?;
    fixture.request().await?;
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("echo: sentinel")
    );
    assert_eq!(server.observed.calls.load(Ordering::SeqCst), 1);
    assert!(
        serve
            .updates("tool_call_update")
            .iter()
            .any(|update| update.get("status").and_then(Value::as_str) == Some("completed"))
    );
    let closed = serve
        .request("session/close", &json!({"sessionId": serve.session_id()}))
        .await?;
    assert!(closed.get("error").is_none(), "{closed}");
    wait_closed(&server).await?;
    serve.disconnect();
    assert!(serve.wait(LIMIT).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn legacy_sse_running_cancellation_recovers_and_disconnect_closes_stream()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let server = LegacyServer::start(None).await?;
    let mut serve = fixture.start(servers(&server)).await?;
    let id = serve.start_prompt("hold").await?;
    let started = serve
        .pump(LIMIT, Some(id), || {
            server.observed.calls.load(Ordering::SeqCst) == 1
        })
        .await?;
    assert!(
        matches!(started, Stopped::Predicate),
        "call did not start: {started:?}"
    );
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    complete(&mut serve, id, None, "cancelled").await?;
    let cancelled = serve
        .pump(LIMIT, None, || {
            server.observed.cancellations.load(Ordering::SeqCst) == 1
        })
        .await?;
    assert!(
        matches!(cancelled, Stopped::Predicate),
        "remote cancellation missing: {cancelled:?}"
    );
    fixture.request().await?;
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("allow_once"), "end_turn").await?;
    fixture.request().await?;
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("echo: sentinel")
    );
    assert_eq!(server.observed.calls.load(Ordering::SeqCst), 2);
    serve.disconnect();
    assert!(serve.wait(LIMIT).await?.success());
    wait_closed(&server).await?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn legacy_sse_invalid_setup_is_sanitized_and_keeps_acp_usable() -> Result<(), Box<dyn Error>>
{
    let fixture = Fixture::new().await?;
    let mut serve = fixture.start(json!([])).await?;
    for (endpoint, behavior) in [
        (
            Some("http://other.invalid/messages?token=MCP_PRIVATE_SENTINEL"),
            Behavior::Normal,
        ),
        (None, Behavior::MalformedMessage),
        (None, Behavior::SilentToolsList),
    ] {
        let server = LegacyServer::with_behavior(endpoint, behavior).await?;
        let response = serve
            .request(
                "session/new",
                &json!({"cwd": fixture.root, "mcpServers": servers(&server)}),
            )
            .await?;
        assert_eq!(
            response.pointer("/error/code").and_then(Value::as_i64),
            Some(-32602)
        );
        assert!(
            !response.to_string().contains("MCP_PRIVATE_SENTINEL"),
            "private configuration reached the editor"
        );
        assert!(!response.to_string().contains(TOKEN));
        wait_closed(&server).await?;
    }
    let id = serve.start_prompt("recovery").await?;
    complete(&mut serve, id, None, "end_turn").await?;
    serve.disconnect();
    assert!(serve.wait(LIMIT).await?.success());
    Ok(())
}
