//! MCP approval and cancellation through the shipped adapter and real transports.

mod acp_client;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

use acp_client::{Serve, Stopped};

const TOOL_NAME: &str = "mcp__audit__stdio_echo";
const LIMIT: Duration = Duration::from_secs(5);

struct Fixture {
    root: PathBuf,
    base_url: String,
    requests: UnboundedReceiver<Value>,
    provider: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!("mcp-permission-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let (sender, requests) = unbounded_channel();
        let router = Router::new()
            .route("/models", get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/json")],
                    json!({"data": [{"id": "fixture-model", "context_window": 8192}]}).to_string())
            }))
            .route("/chat/completions", post(move |body: String| {
                let sender = sender.clone();
                async move {
                    let request: Value = serde_json::from_str(&body)
                        .map_err(|_| (axum::http::StatusCode::BAD_REQUEST, "invalid fixture request"))?;
                    let last = request.get("messages").and_then(Value::as_array).and_then(|m| m.last());
                    let tool_turn = last.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user");
                    let hold = last.and_then(|m| m.get("content")).and_then(Value::as_str) == Some("hold");
                    let delta = if tool_turn {
                        json!({"tool_calls": [{"index": 0, "id": "fixture-call", "type": "function",
                            "function": {"name": TOOL_NAME, "arguments": json!({"message": if hold {"hold"} else {"sentinel"}}).to_string()}}]})
                    } else {
                        json!({"content": "done"})
                    };
                    // The receiver belongs to this test; a dropped test needs no response evidence.
                    let _ = sender.send(request);
                    let frame = json!({"choices": [{"index": 0, "delta": delta,
                        "finish_reason": if tool_turn {"tool_calls"} else {"stop"}}]});
                    Ok::<_, (axum::http::StatusCode, &str)>(([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        format!("data: {frame}\n\ndata: [DONE]\n\n")))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let provider = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "local provider fixture failed");
            }
        });
        Ok(Self {
            root,
            base_url,
            requests,
            provider,
        })
    }

    async fn start(&self, mode: &str) -> Result<Serve, Box<dyn Error>> {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        let mcp = binary
            .parent()
            .ok_or("binary has no parent")?
            .join("examples")
            .join(format!("mcp_stdio_fixture{}", std::env::consts::EXE_SUFFIX));
        assert!(
            mcp.is_file(),
            "cargo test must build the stdio fixture example"
        );
        let mut command = Command::new(binary);
        command
            .args(["serve", "--backend", "groq"])
            .env("LLM_API_KEY", "fixture-key")
            .env("LLM_BASE_URL", &self.base_url)
            .env("LLM_MODEL", "fixture-model")
            .env("XDG_STATE_HOME", &self.root)
            .env_remove("ACP_LOG");
        let mut serve = Serve::start_with(
            command,
            json!({"cwd": self.root, "mcpServers": [{
                "name": "audit", "command": mcp, "args": ["fixture-arg"], "env": [
                    {"name": "ACP_LLM_ADAPTER_RUN_MCP_FIXTURE", "value": "1"},
                    {"name": "MCP_FIXTURE_TOKEN", "value": "fixture-env"},
                    {"name": "MCP_FIXTURE_INVOCATION_LOG", "value": self.root.join("invocations")}
                ]
            }]}),
        )
        .await?;
        let response = serve
            .request(
                "session/set_mode",
                &json!({"sessionId": serve.session_id(), "modeId": mode}),
            )
            .await?;
        assert!(
            response.get("error").is_none(),
            "mode change failed: {response}"
        );
        Ok(serve)
    }

    async fn request(&mut self) -> Result<Value, Box<dyn Error>> {
        tokio::time::timeout(LIMIT, self.requests.recv())
            .await?
            .ok_or_else(|| "provider stopped".into())
    }

    fn events(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("invocations"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.provider.abort();
        // This uniquely allocated test directory owns no user artifacts.
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            tracing::error!(%error, "could not remove MCP test state");
        }
    }
}

async fn complete(
    serve: &mut Serve,
    id: u64,
    option: Option<&str>,
    stop: &str,
) -> Result<(), Box<dyn Error>> {
    let outcome = serve
        .pump_with_permission(LIMIT, Some(id), || false, option)
        .await?;
    let Stopped::Response(response) = outcome else {
        return Err(format!("prompt did not complete: {outcome:?}").into());
    };
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some(stop),
        "{response}"
    );
    Ok(())
}

async fn stop(serve: &mut Serve) -> Result<(), Box<dyn Error>> {
    serve.disconnect();
    assert!(serve.wait(LIMIT).await?.success());
    Ok(())
}

fn permission_count(serve: &Serve) -> usize {
    serve
        .received
        .iter()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("session/request_permission"))
        .count()
}

async fn assert_tool_advertised(fixture: &mut Fixture) -> Result<(), Box<dyn Error>> {
    let request = fixture.request().await?;
    assert!(
        request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools
                .iter()
                .any(|t| t.pointer("/function/name").and_then(Value::as_str) == Some(TOOL_NAME)))
    );
    Ok(())
}

#[test_log::test(tokio::test)]
async fn ask_rejects_without_invocation_then_allows_once() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("ask").await?;
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("reject_once"), "end_turn").await?;
    assert_tool_advertised(&mut fixture).await?;
    let request = fixture.request().await?;
    assert_eq!(
        permission_count(&serve),
        1,
        "MCP dispatch bypassed editor approval"
    );
    assert!(
        fixture.events().is_empty(),
        "rejected MCP call reached the server"
    );
    assert!(
        request
            .to_string()
            .contains("rejected by permission policy")
    );
    assert!(
        serve
            .updates("tool_call_update")
            .iter()
            .any(|u| u.get("status").and_then(Value::as_str) == Some("failed"))
    );

    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("allow_once"), "end_turn").await?;
    assert_tool_advertised(&mut fixture).await?;
    let request = fixture.request().await?;
    assert_eq!(permission_count(&serve), 2);
    assert_eq!(fixture.events(), ["invoked"]);
    assert!(
        request
            .to_string()
            .contains("stdio echo: sentinel; arg: fixture-arg; env: fixture-env")
    );
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn accept_edits_still_asks_for_mcp_and_yolo_allows_it() -> Result<(), Box<dyn Error>> {
    for (mode, count) in [("accept-edits", 1), ("yolo", 0)] {
        let mut fixture = Fixture::new().await?;
        let mut serve = fixture.start(mode).await?;
        let id = serve.start_prompt("echo").await?;
        complete(&mut serve, id, Some("allow_once"), "end_turn").await?;
        assert_tool_advertised(&mut fixture).await?;
        fixture.request().await?;
        assert_eq!(
            permission_count(&serve),
            count,
            "wrong MCP approval policy in {mode}"
        );
        assert_eq!(fixture.events(), ["invoked"]);
        stop(&mut serve).await?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn remembered_allow_skips_approval_but_cannot_override_plan() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("ask").await?;
    for option in [Some("allow_always"), None] {
        let id = serve.start_prompt("echo").await?;
        complete(&mut serve, id, option, "end_turn").await?;
        assert_tool_advertised(&mut fixture).await?;
        fixture.request().await?;
    }
    assert_eq!(permission_count(&serve), 1);
    assert_eq!(fixture.events(), ["invoked", "invoked"]);
    // A remembered approval cannot override Plan's execution prohibition.
    serve
        .request(
            "session/set_mode",
            &json!({"sessionId": serve.session_id(), "modeId": "plan"}),
        )
        .await?;
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, None, "end_turn").await?;
    let request = fixture.request().await?;
    assert!(
        !request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools
                .iter()
                .any(|t| t.pointer("/function/name").and_then(Value::as_str) == Some(TOOL_NAME)))
    );
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("plan mode refuses")
    );
    assert_eq!(fixture.events(), ["invoked", "invoked"]);
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn remembered_rejection_survives_later_prompts_and_yolo() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("ask").await?;
    for (mode, option) in [
        ("ask", Some("reject_always")),
        ("ask", None),
        ("yolo", None),
    ] {
        serve
            .request(
                "session/set_mode",
                &json!({"sessionId": serve.session_id(), "modeId": mode}),
            )
            .await?;
        let id = serve.start_prompt("echo").await?;
        complete(&mut serve, id, option, "end_turn").await?;
        assert_tool_advertised(&mut fixture).await?;
        assert!(
            fixture
                .request()
                .await?
                .to_string()
                .contains("rejected by permission policy")
        );
        assert!(fixture.events().is_empty());
    }
    assert_eq!(permission_count(&serve), 1);
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn selected_content_refuses_provider_selected_mcp_tools_even_in_yolo()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("ask").await?;
    let created = serve.request("session/new", &json!({"cwd": fixture.root, "mcpServers": [],
        "_meta": {"io.github.euri10.louiselm.selectedContent": {
            "version": 1, "input_bytes": 1024, "output_bytes": 1024, "max_tokens": 128, "timeout_ms": 5000
        }}})).await?;
    let session_id = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("selected-content session failed")?;
    serve
        .request(
            "session/set_mode",
            &json!({"sessionId": session_id, "modeId": "yolo"}),
        )
        .await?;
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "echo"}]}),
        )
        .await?;
    assert!(
        response.get("error").is_some(),
        "selected-content accepted an MCP tool delta: {response}"
    );
    let request = fixture.request().await?;
    assert!(
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    assert!(
        fixture.requests.try_recv().is_err(),
        "selected-content issued a tool follow-up"
    );
    assert_eq!(permission_count(&serve), 0);
    assert!(fixture.events().is_empty());
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn cancellation_while_approval_is_pending_leaves_mcp_untouched() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("ask").await?;
    let id = serve.start_prompt("echo").await?;
    let pending = serve
        .pump_with_permission(LIMIT, Some(id), || false, None)
        .await?;
    let Stopped::Permission(permission) = pending else {
        return Err(format!("expected MCP approval, got {pending:?}").into());
    };
    assert_tool_advertised(&mut fixture).await?;
    assert!(fixture.events().is_empty());
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    complete(&mut serve, id, None, "cancelled").await?;
    assert!(fixture.events().is_empty());
    // A stale editor reply must not start the cancelled tool.
    serve
        .select_permission(
            permission.get("id").ok_or("permission id missing")?,
            "allow_once",
        )
        .await?;
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, Some("allow_once"), "end_turn").await?;
    assert_tool_advertised(&mut fixture).await?;
    fixture.request().await?;
    assert_eq!(fixture.events(), ["invoked"]);
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn cancellation_reaches_the_pending_mcp_call_and_session_stays_usable()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start("yolo").await?;
    let id = serve.start_prompt("hold").await?;
    assert_tool_advertised(&mut fixture).await?;
    let waited = serve
        .pump(LIMIT, Some(id), || fixture.events() == ["invoked"])
        .await?;
    assert!(
        matches!(waited, Stopped::Predicate),
        "fixture call never started: {waited:?}"
    );
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    complete(&mut serve, id, None, "cancelled").await?;
    let waited = serve
        .pump(LIMIT, None, || {
            fixture.events().contains(&"cancelled".to_string())
        })
        .await?;
    assert!(
        matches!(waited, Stopped::Predicate),
        "MCP cancellation never reached the server: {waited:?}"
    );
    let id = serve.start_prompt("echo").await?;
    complete(&mut serve, id, None, "end_turn").await?;
    assert_tool_advertised(&mut fixture).await?;
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("stdio echo: sentinel")
    );
    assert_eq!(fixture.events(), ["invoked", "cancelled", "invoked"]);
    stop(&mut serve).await
}
