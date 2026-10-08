//! A genuine legacy MCP server: GET events and separate POST messages.

// Unit and wire tests share this fixture but exercise different failure modes.
#![allow(dead_code)]

use std::convert::Infallible;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response, Sse, sse::Event};
use axum::routing::{get, post};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

pub(crate) const TOKEN: &str = "Bearer legacy-fixture-secret";

#[derive(Clone, Copy, Default)]
pub(crate) enum Behavior {
    #[default]
    Normal,
    MissingEndpoint,
    SilentInitialize,
    SilentToolsList,
    MalformedMessage,
    RedirectPost,
    RedirectGet,
}

#[derive(Default)]
pub(crate) struct Observed {
    pub(crate) gets: AtomicUsize,
    pub(crate) posts: AtomicUsize,
    pub(crate) calls: AtomicUsize,
    pub(crate) cancellations: AtomicUsize,
    pub(crate) active: AtomicUsize,
    pub(crate) redirects: AtomicUsize,
}

struct ServerState {
    observed: Arc<Observed>,
    endpoint: String,
    sender: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    behavior: Behavior,
}

pub(crate) struct LegacyServer {
    pub(crate) url: String,
    pub(crate) observed: Arc<Observed>,
    task: JoinHandle<()>,
}

impl LegacyServer {
    pub(crate) async fn start(endpoint: Option<&str>) -> Result<Self, Box<dyn Error>> {
        Self::with_behavior(endpoint, Behavior::Normal).await
    }

    pub(crate) async fn with_behavior(
        endpoint: Option<&str>,
        behavior: Behavior,
    ) -> Result<Self, Box<dyn Error>> {
        let observed = Arc::new(Observed::default());
        let state = Arc::new(ServerState {
            observed: observed.clone(),
            endpoint: endpoint.unwrap_or("/messages?session=legacy").to_string(),
            sender: Mutex::new(None),
            behavior,
        });
        let router = axum::Router::new()
            .route("/sse", get(events))
            .route("/messages", post(messages))
            .route(
                "/must-not-follow",
                get(redirect_target).post(redirect_target),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/sse", listener.local_addr()?);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "legacy MCP fixture failed");
            }
        });
        Ok(Self {
            url,
            observed,
            task,
        })
    }
}

impl Drop for LegacyServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Active(Arc<Observed>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn events(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.observed.gets.fetch_add(1, Ordering::SeqCst);
    if matches!(state.behavior, Behavior::RedirectGet) {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/must-not-follow")],
        )
            .into_response();
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    *state.sender.lock().await = Some(sender);
    state.observed.active.fetch_add(1, Ordering::SeqCst);
    let guard = Active(state.observed.clone());
    let endpoint = state.endpoint.clone();
    let behavior = state.behavior;
    let stream = stream::once(async move {
        Ok::<_, Infallible>(if matches!(behavior, Behavior::MissingEndpoint) {
            Event::default().comment("waiting")
        } else {
            Event::default().event("endpoint").data(endpoint)
        })
    })
    .chain(stream::unfold(
        (receiver, guard),
        move |(mut receiver, guard)| async move {
            let message = receiver.recv().await?;
            let data = if matches!(behavior, Behavior::MalformedMessage) {
                "MCP_PRIVATE_SENTINEL invalid JSON".to_string()
            } else {
                message.to_string()
            };
            Some((
                Ok(Event::default().event("message").data(data)),
                (receiver, guard),
            ))
        },
    ));
    Sse::new(stream).into_response()
}

async fn messages(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.observed.posts.fetch_add(1, Ordering::SeqCst);
    if matches!(state.behavior, Behavior::RedirectPost) {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/must-not-follow")],
        )
            .into_response();
    }
    let Ok(request) = serde_json::from_str::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if matches!(state.behavior, Behavior::SilentInitialize)
        || (matches!(state.behavior, Behavior::SilentToolsList)
            && request.get("method").and_then(Value::as_str) == Some("tools/list"))
    {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
            "serverInfo": {"name": "legacy-fixture", "version": "1"}
        }),
        Some("tools/list") => json!({"tools": [{"name": "echo", "description": "Echo text",
            "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                "required": ["message"]}}]}),
        Some("tools/call") => {
            state.observed.calls.fetch_add(1, Ordering::SeqCst);
            let message = request
                .pointer("/params/arguments/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            if message == "hold" {
                return StatusCode::ACCEPTED.into_response();
            }
            json!({"content": [{"type": "text", "text": format!("echo: {message}")}]})
        }
        Some("notifications/cancelled") => {
            state.observed.cancellations.fetch_add(1, Ordering::SeqCst);
            return StatusCode::ACCEPTED.into_response();
        }
        Some("notifications/initialized") => return StatusCode::ACCEPTED.into_response(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let response = json!({"jsonrpc": "2.0", "id": request.get("id"), "result": result});
    if let Some(sender) = state.sender.lock().await.as_ref() {
        // A cancelled/dropped client no longer needs its synthetic response.
        let _ = sender.send(response);
    }
    StatusCode::ACCEPTED.into_response()
}

async fn redirect_target(State(state): State<Arc<ServerState>>) -> Response {
    state.observed.redirects.fetch_add(1, Ordering::SeqCst);
    StatusCode::BAD_REQUEST.into_response()
}
