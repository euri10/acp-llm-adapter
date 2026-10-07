//! Completion replay policy must hold at the HTTP sender behind shipped `serve`.

mod acp_client;

use std::error::Error;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use futures_util::{StreamExt as _, stream};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinSet;

use acp_client::{Serve, Stopped};
use acp_llm_adapter::llm::{
    ChatClient, ChatConfig, ChatMessage, ChatRequest, FinishReason, LlmClient, StreamEvent,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum FirstReply {
    Drop(String),
    Pending {
        started: Arc<Notify>,
        closed: Arc<Notify>,
    },
    Redirect,
}

struct StreamClosed(Arc<Notify>);

impl Drop for StreamClosed {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

async fn replay_provider(
    first_reply: FirstReply,
    recover: bool,
) -> Result<(String, Arc<AtomicUsize>, JoinSet<std::io::Result<()>>), Box<dyn Error>> {
    let posts = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/models", get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({"data":[{"id":"fixture-model"}]}).to_string())
        }))
        .route("/chat/completions", post(|
            State((posts, first_reply, recover)): State<(Arc<AtomicUsize>, FirstReply, bool)>,
            request: String,
        | async move {
            let attempt = posts.fetch_add(1, Ordering::SeqCst);
            let max_tokens = serde_json::from_str::<Value>(&request).ok()
                .and_then(|body| body.get("max_tokens").and_then(Value::as_u64));
            if let Some(max_tokens) = max_tokens {
                assert_eq!(max_tokens, 64, "the immutable token cap did not reach HTTP");
            }
            if attempt > 0 && recover {
                let chunk = json!({"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}]});
                return ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")).into_response();
            }
            match first_reply {
                FirstReply::Drop(body) => (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")], body
                ).into_response(),
                FirstReply::Pending { started, closed } => {
                    let closed = StreamClosed(closed);
                    let body = stream::once(async {
                        Ok::<_, std::io::Error>("retry: 10\n\n")
                    }).chain(stream::pending()).map(move |part| {
                        let _keep_until_stream_drop = &closed;
                        part
                    });
                    let mut response = Response::new(Body::from_stream(body));
                    response.headers_mut().insert(axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("text/event-stream"));
                    started.notify_one();
                    response
                }
                FirstReply::Redirect => (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, "/chat/completions")],
                ).into_response(),
            }
        }))
        .with_state((Arc::clone(&posts), first_reply, recover));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut server = JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    Ok((url, posts, server))
}

fn serve_command(base_url: &str, state_dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "fixture-model")
        .env("XDG_STATE_HOME", state_dir)
        .env_remove("ACP_LOG");
    command
}

fn selected_session(timeout_ms: u64) -> Value {
    json!({"cwd":"/tmp", "mcpServers":[], "_meta":{
        "io.github.euri10.louiselm.selectedContent":{
            "version":1, "input_bytes":1024, "output_bytes":128,
            "max_tokens":64, "timeout_ms":timeout_ms
        }
    }})
}

async fn check_selected_content_drop(partial_output: bool) -> Result<(), Box<dyn Error>> {
    let mut first_body = "retry: 10\n\n".to_string();
    if partial_output {
        let chunk = json!({"choices":[{"delta":{"content":"first "},"finish_reason":null}]});
        write!(first_body, "data: {chunk}\n\n")?;
    }
    let (url, posts, mut server) = replay_provider(FirstReply::Drop(first_body), true).await?;
    let state_dir = std::env::temp_dir().join(format!("acp-stream-retry-{}", uuid::Uuid::new_v4()));
    let mut serve =
        Serve::start_with(serve_command(&url, &state_dir), selected_session(4000)).await?;
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId":serve.session_id(),
            "prompt":[{"type":"text","text":"private selected snapshot"}]}),
        )
        .await?;
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "one selected prompt caused multiple HTTP POSTs"
    );
    assert!(
        response.get("error").is_some(),
        "an incomplete generation succeeded: {response}"
    );
    let text: Vec<&str> = serve
        .updates("agent_message_chunk")
        .into_iter()
        .filter_map(|update| update.pointer("/content/text").and_then(Value::as_str))
        .collect();
    assert_eq!(
        text,
        if partial_output {
            vec!["first "]
        } else {
            vec![]
        }
    );
    assert!(
        serve.updates("usage_update").is_empty(),
        "absent usage must remain unknown"
    );
    let persisted = state_dir
        .join("acp-llm-adapter/sessions")
        .join(serve.session_id());
    assert!(
        !persisted.join("history.jsonl").exists(),
        "helper history was persisted"
    );
    assert!(
        !persisted.join("meta.json").exists(),
        "helper title was persisted"
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "the stream task outlived its consumer"
    );
    server.shutdown().await;
    if state_dir.exists() {
        std::fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn selected_content_never_replays_a_drop_before_output() -> Result<(), Box<dyn Error>> {
    check_selected_content_drop(false).await
}

#[test_log::test(tokio::test)]
async fn selected_content_never_appends_a_replayed_generation() -> Result<(), Box<dyn Error>> {
    check_selected_content_drop(true).await
}

#[test_log::test(tokio::test)]
async fn ordinary_stream_recovers_only_before_any_completion_event() -> Result<(), Box<dyn Error>> {
    for delta in [
        json!({}),
        json!({"content":"first "}),
        json!({"reasoning_content":"thinking"}),
        json!({"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{"}}]}),
    ] {
        let pre_output = delta == json!({});
        let chunk = json!({"choices":[{"delta":delta,"finish_reason":null}]});
        let (url, posts, mut server) = replay_provider(
            FirstReply::Drop(format!("retry: 10\n\ndata: {chunk}\n\n")),
            true,
        )
        .await?;
        let state_dir =
            std::env::temp_dir().join(format!("acp-stream-retry-{}", uuid::Uuid::new_v4()));
        let mut serve = Serve::start_with(
            serve_command(&url, &state_dir),
            json!({"cwd":"/tmp","mcpServers":[]}),
        )
        .await?;
        let response = serve
            .request(
                "session/prompt",
                &json!({"sessionId":serve.session_id(),
            "prompt":[{"type":"text","text":"hello"}]}),
            )
            .await?;
        if pre_output {
            assert_eq!(
                posts.load(Ordering::SeqCst),
                2,
                "safe pre-output recovery was lost"
            );
            assert_eq!(
                response
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str),
                Some("end_turn")
            );
        } else {
            assert_eq!(
                posts.load(Ordering::SeqCst),
                1,
                "a generation was replayed after an emitted event"
            );
            assert!(
                response.get("error").is_some(),
                "a truncated generation succeeded: {response}"
            );
            assert!(
                serve
                    .updates("agent_message_chunk")
                    .into_iter()
                    .all(
                        |update| update.pointer("/content/text").and_then(Value::as_str)
                            != Some("second")
                    )
            );
        }
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
        server.shutdown().await;
        std::fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn ordinary_stream_exhausts_a_finite_retry_budget() -> Result<(), Box<dyn Error>> {
    let (url, posts, mut server) =
        replay_provider(FirstReply::Drop("retry: 10\n\n".into()), false).await?;
    let state_dir = std::env::temp_dir().join(format!("acp-stream-retry-{}", uuid::Uuid::new_v4()));
    let mut serve = Serve::start_with(
        serve_command(&url, &state_dir),
        json!({"cwd":"/tmp","mcpServers":[]}),
    )
    .await?;
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId":serve.session_id(),
        "prompt":[{"type":"text","text":"hello"}]}),
        )
        .await?;
    assert!(
        response.get("error").is_some(),
        "retry exhaustion did not fail: {response}"
    );
    assert_eq!(
        posts.load(Ordering::SeqCst),
        4,
        "expected the initial send and three retries"
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    std::fs::remove_dir_all(state_dir)?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn selected_content_deadline_and_cancel_cannot_resend() -> Result<(), Box<dyn Error>> {
    for cancel in [false, true] {
        let started = Arc::new(Notify::new());
        let closed = Arc::new(Notify::new());
        let (url, posts, mut server) = replay_provider(
            FirstReply::Pending {
                started: Arc::clone(&started),
                closed: Arc::clone(&closed),
            },
            true,
        )
        .await?;
        let state_dir =
            std::env::temp_dir().join(format!("acp-stream-retry-{}", uuid::Uuid::new_v4()));
        let timeout_ms = if cancel { 4000 } else { 1000 };
        let mut serve = Serve::start_with(
            serve_command(&url, &state_dir),
            selected_session(timeout_ms),
        )
        .await?;
        let prompt = serve.start_prompt("private selected snapshot").await?;
        tokio::time::timeout(Duration::from_secs(3), started.notified()).await?;
        if cancel {
            serve
                .notify("session/cancel", &json!({"sessionId":serve.session_id()}))
                .await?;
        }
        let result = serve
            .pump(Duration::from_secs(3), Some(prompt), || false)
            .await?;
        let Stopped::Response(response) = result else {
            return Err(format!("pending completion did not terminate: {result:?}").into());
        };
        if cancel {
            assert_eq!(
                response
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str),
                Some("cancelled")
            );
        } else {
            assert_eq!(
                response.pointer("/error/data").and_then(Value::as_str),
                Some("selected-content deadline exceeded")
            );
        }
        assert!(serve.updates("agent_message_chunk").is_empty());
        assert!(serve.updates("usage_update").is_empty());
        tokio::time::timeout(Duration::from_secs(3), closed.notified())
            .await
            .map_err(|_| "the provider response outlived the cancelled/deadline turn")?;
        let persisted = state_dir
            .join("acp-llm-adapter/sessions")
            .join(serve.session_id());
        assert!(!persisted.join("history.jsonl").exists());
        assert!(!persisted.join("meta.json").exists());
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "cancellation/deadline caused a resend"
        );
        server.shutdown().await;
        if state_dir.exists() {
            std::fs::remove_dir_all(state_dir)?;
        }
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn selected_content_redirect_cannot_send_a_second_post() -> Result<(), Box<dyn Error>> {
    let (url, posts, mut server) = replay_provider(FirstReply::Redirect, true).await?;
    let state_dir = std::env::temp_dir().join(format!("acp-stream-retry-{}", uuid::Uuid::new_v4()));
    let mut serve =
        Serve::start_with(serve_command(&url, &state_dir), selected_session(4000)).await?;
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId":serve.session_id(),
        "prompt":[{"type":"text","text":"hello"}]}),
        )
        .await?;
    assert!(response.get("error").is_some());
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "HTTP redirected a single-send completion POST"
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    if state_dir.exists() {
        std::fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn cancelling_or_dropping_a_client_stream_closes_its_pending_http_response()
-> Result<(), Box<dyn Error>> {
    for cancel in [false, true] {
        let started = Arc::new(Notify::new());
        let closed = Arc::new(Notify::new());
        let (url, posts, mut server) = replay_provider(
            FirstReply::Pending {
                started: Arc::clone(&started),
                closed: Arc::clone(&closed),
            },
            true,
        )
        .await?;
        let client = ChatClient::new(ChatConfig::new("fixture-key", url, "fixture-model"));
        let token = CancellationToken::new();
        let mut response = client.stream_chat(
            ChatRequest::new(vec![ChatMessage::user("hello")]),
            token.clone(),
        )?;
        tokio::time::timeout(Duration::from_secs(3), started.notified()).await?;
        if cancel {
            token.cancel();
            assert!(
                tokio::time::timeout(Duration::from_secs(3), response.next())
                    .await?
                    .is_none()
            );
        }
        drop(response);
        tokio::time::timeout(Duration::from_secs(3), closed.notified())
            .await
            .map_err(|_| "the pending HTTP response outlived its client stream")?;
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        server.shutdown().await;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn a_finish_reason_completes_without_done_or_replay() -> Result<(), Box<dyn Error>> {
    let chunk = json!({"choices":[{"delta":{"content":"complete"},"finish_reason":"stop"}]});
    for single_send in [false, true] {
        let (url, posts, mut server) =
            replay_provider(FirstReply::Drop(format!("data: {chunk}\n\n")), true).await?;
        let client = ChatClient::new(ChatConfig::new("fixture-key", url, "fixture-model"));
        let mut request = ChatRequest::new(vec![ChatMessage::user("hello")]);
        if single_send {
            request = request.without_retries();
        }
        let mut response = client.stream_chat(request, CancellationToken::new())?;
        let events = tokio::time::timeout(Duration::from_secs(3), async move {
            let mut events = Vec::new();
            while let Some(event) = response.next().await {
                events.push(event?);
            }
            Ok::<_, Box<dyn Error>>(events)
        })
        .await??;
        assert_eq!(
            events,
            vec![
                StreamEvent::Message("complete".into()),
                StreamEvent::Finished(FinishReason::EndTurn),
            ]
        );
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "a finished generation was replayed"
        );
        server.shutdown().await;
    }
    Ok(())
}
