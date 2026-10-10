//! Protocol version negotiation on the shipped `serve` binary.
//!
//! Without the `protocol-v2` feature the adapter speaks ACP v1 only, so a
//! client asking for v2 is answered with v1. With it, v1 and v2 connections are
//! routed to separate implementations; v1 is unchanged either way.
#![allow(
    clippy::indexing_slicing,
    reason = "Test assertions index JSON and update lists deliberately; a panic is a failure."
)]

use std::error::Error;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Spawn `serve` on the mock backend and return its reply to one `initialize`.
async fn initialize(params: Value) -> Result<Value, Box<dyn Error>> {
    let state_dir = std::env::temp_dir().join(format!("acp-v2-test-{}", uuid::Uuid::new_v4()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .args(["serve", "--backend", "mock"])
        .env("XDG_STATE_HOME", &state_dir)
        .env_remove("ACP_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
    let mut lines = BufReader::new(child.stdout.take().ok_or("serve exposed no stdout")?).lines();
    let request = json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":params});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or("serve closed stdout before answering initialize")?;
            let response: Value = serde_json::from_str(&line)?;
            if response.get("id").and_then(Value::as_u64) == Some(1) {
                return Ok::<_, Box<dyn Error>>(response);
            }
        }
    })
    .await??;
    drop(stdin);
    child.wait().await?;
    if state_dir.exists() {
        std::fs::remove_dir_all(state_dir)?;
    }
    Ok(response)
}

fn v1_request() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}
    })
}

fn v2_request() -> Value {
    json!({
        "protocolVersion": 2,
        "info": {"name": "serve-protocol-v2-test", "version": "0.0.0"},
        "capabilities": {}
    })
}

#[tokio::test]
async fn v1_clients_get_the_unchanged_v1_handshake() -> Result<(), Box<dyn Error>> {
    let response = initialize(v1_request()).await?;
    let at = |path: &str| response.pointer(path).cloned();
    assert_eq!(at("/result/protocolVersion"), Some(json!(1)), "{response}");
    assert_eq!(
        at("/result/agentCapabilities/loadSession"),
        Some(json!(true))
    );
    assert_eq!(at("/result/agentInfo/name"), Some(json!("acp-llm-adapter")));
    Ok(())
}

#[cfg(not(feature = "protocol-v2"))]
#[tokio::test]
async fn without_the_feature_v2_requests_get_v1() -> Result<(), Box<dyn Error>> {
    let response = initialize(v2_request()).await?;
    assert_eq!(
        response.pointer("/result/protocolVersion"),
        Some(&json!(1)),
        "{response}"
    );
    Ok(())
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn with_the_feature_v2_requests_get_the_v2_baseline() -> Result<(), Box<dyn Error>> {
    let response = initialize(v2_request()).await?;
    let result = response.get("result").ok_or("v2 initialize failed")?;
    let at = |path: &str| result.pointer(path).cloned();
    assert_eq!(at("/protocolVersion"), Some(json!(2)), "{response}");
    assert_eq!(at("/info/name"), Some(json!("acp-llm-adapter")));
    assert_eq!(at("/info/version"), Some(json!(env!("CARGO_PKG_VERSION"))));
    // The whole baseline session surface; no MCP, prompt extensions or
    // selected-content extension in the probe.
    assert_eq!(
        at("/capabilities"),
        Some(json!({"session": {}})),
        "{response}"
    );
    assert!(
        result
            .get("authMethods")
            .is_none_or(|methods| methods == &json!([]))
    );
    assert!(result.get("_meta").is_none(), "{response}");
    Ok(())
}

/// One `serve --backend mock` connection, collecting every notification.
#[cfg(feature = "protocol-v2")]
struct V2Connection {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    notifications: Vec<Value>,
    next_id: u64,
    state_dir: std::path::PathBuf,
}

#[cfg(feature = "protocol-v2")]
impl V2Connection {
    async fn start() -> Result<Self, Box<dyn Error>> {
        Self::start_with(&["serve", "--backend", "mock"], &[]).await
    }

    async fn start_with(args: &[&str], envs: &[(&str, &str)]) -> Result<Self, Box<dyn Error>> {
        let state_dir =
            std::env::temp_dir().join(format!("acp-v2-session-{}", uuid::Uuid::new_v4()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(args)
            .envs(envs.iter().copied())
            .env("XDG_STATE_HOME", &state_dir)
            .env_remove("ACP_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
        let lines = BufReader::new(child.stdout.take().ok_or("serve exposed no stdout")?).lines();
        let mut connection = Self {
            child,
            stdin,
            lines,
            notifications: Vec::new(),
            next_id: 1,
            state_dir,
        };
        let initialized = connection.request("initialize", v2_request()).await?;
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!(2))
        );
        Ok(connection)
    }

    /// Send a request and return its response, keeping notifications.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        loop {
            let message = self.next_message().await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(message);
            }
        }
    }

    async fn next_message(&mut self) -> Result<Value, Box<dyn Error>> {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await??
            .ok_or("serve closed stdout")?;
        let message: Value = serde_json::from_str(&line)?;
        if message.get("method").and_then(Value::as_str) == Some("session/update") {
            self.notifications.push(message.clone());
        }
        Ok(message)
    }

    /// Read until a session reports `state: idle`, returning the updates seen
    /// since `from` (inclusive of the idle update).
    async fn until_idle(&mut self, from: usize) -> Result<Vec<Value>, Box<dyn Error>> {
        while !self.notifications[from..]
            .iter()
            .any(|update| update.pointer("/params/update/state") == Some(&json!("idle")))
        {
            self.next_message().await?;
        }
        Ok(self.notifications[from..]
            .iter()
            .filter_map(|message| message.pointer("/params/update").cloned())
            .collect())
    }

    async fn stop(mut self) -> Result<(), Box<dyn Error>> {
        drop(self.stdin);
        self.child.wait().await?;
        if self.state_dir.exists() {
            std::fs::remove_dir_all(&self.state_dir)?;
        }
        Ok(())
    }
}

#[cfg(feature = "protocol-v2")]
fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            let kind = update["sessionUpdate"].as_str().unwrap_or_default();
            match update.get("state").and_then(Value::as_str) {
                Some(state) => format!("{kind}:{state}"),
                None => kind.to_owned(),
            }
        })
        .collect()
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_session_baseline_over_the_wire() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    let cwd = std::env::temp_dir();

    // session/new: commands in the response, then the session reports idle.
    let created = connection
        .request("session/new", json!({"cwd": cwd}))
        .await?;
    let session_id = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("session/new failed: {created}"))?
        .to_owned();
    assert!(
        created
            .pointer("/result/availableCommands")
            .and_then(Value::as_array)
            .is_some_and(|commands| !commands.is_empty())
    );
    let ready = connection.until_idle(0).await?;
    assert_eq!(kinds(&ready), ["state_update:idle"]);

    // session/prompt: accepted with the user message ID, then the turn streams.
    let mark = connection.notifications.len();
    let prompt = json!([
        {"type": "text", "text": "hello"},
        {"type": "resource_link", "name": "notes", "uri": "file:///tmp/notes.md"}
    ]);
    let accepted = connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": prompt}),
        )
        .await?;
    let message_id = accepted
        .pointer("/result/messageId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("session/prompt was not accepted: {accepted}"))?
        .to_owned();
    let turn = connection.until_idle(mark).await?;
    let turn_kinds = kinds(&turn);
    assert_eq!(
        turn_kinds.first().map(String::as_str),
        Some("user_message"),
        "{turn_kinds:?}"
    );
    assert_eq!(turn[0]["messageId"], json!(message_id));
    assert_eq!(turn[0]["content"], prompt);
    assert_eq!(
        turn_kinds.get(1).map(String::as_str),
        Some("state_update:running")
    );
    assert!(turn_kinds.contains(&"agent_thought_chunk".to_owned()));
    assert!(
        turn.iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"]
                    == "mock response to: hello\n[notes](file:///tmp/notes.md)")
    );
    let last = turn.last().ok_or("no updates")?;
    assert_eq!(last["state"], "idle");
    assert_eq!(last["stopReason"], "end_turn");

    // session/list sees it; session/resume replays it with the same IDs.
    let listed = connection
        .request("session/list", json!({"cwd": cwd}))
        .await?;
    assert!(
        listed
            .pointer("/result/sessions")
            .and_then(Value::as_array)
            .is_some_and(|sessions| sessions
                .iter()
                .any(|session| session["sessionId"] == json!(session_id))),
        "{listed}"
    );
    let mark = connection.notifications.len();
    let resumed = connection
        .request(
            "session/resume",
            json!({"sessionId": session_id, "cwd": cwd, "replayFrom": {"type": "start"}}),
        )
        .await?;
    assert!(resumed.get("result").is_some(), "{resumed}");
    let replayed: Vec<Value> = connection.notifications[mark..]
        .iter()
        .filter_map(|message| message.pointer("/params/update").cloned())
        .collect();
    assert_eq!(kinds(&replayed), ["user_message", "agent_message"]);
    assert_eq!(replayed[0]["messageId"], json!(message_id));

    // session/close answers {}, and the closed session takes no prompt.
    let closed = connection
        .request("session/close", json!({"sessionId": session_id}))
        .await?;
    assert_eq!(closed.get("result"), Some(&json!({})), "{closed}");
    let refused = connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"again"}]}),
        )
        .await?;
    assert!(refused.get("error").is_some(), "{refused}");
    connection.stop().await
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_refuses_unadvertised_input_before_admission() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    let cwd = std::env::temp_dir();
    let with_mcp = connection
        .request(
            "session/new",
            json!({"cwd": cwd, "mcpServers": [{"name":"x","command":"/bin/true","args":[],"env":[]}]}),
        )
        .await?;
    assert!(with_mcp.get("error").is_some(), "{with_mcp}");

    let created = connection
        .request("session/new", json!({"cwd": cwd}))
        .await?;
    let session_id = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("session/new failed")?
        .to_owned();
    let mark = connection.notifications.len();
    let image = connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [
                {"type":"image","data":"AAAA","mimeType":"image/png"}
            ]}),
        )
        .await?;
    assert!(image.get("error").is_some(), "{image}");
    // Refused before admission: no message was inserted.
    assert!(
        connection.notifications[mark..]
            .iter()
            .all(|message| message.pointer("/params/update/sessionUpdate")
                != Some(&json!("user_message")))
    );
    connection.stop().await
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_reports_failures_after_admission_as_an_error_stop_reason() -> Result<(), Box<dyn Error>>
{
    use axum::routing::{get, post};
    const SENTINEL: &str = "V2_PROVIDER_PRIVATE_SENTINEL";
    let router = axum::Router::new()
        .route(
            "/models",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    json!({"data":[{"id":"fixture-model"}]}).to_string(),
                )
            }),
        )
        .route(
            "/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    format!("{{\"error\":\"{SENTINEL}\"}}"),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    // JoinSet owns and aborts the fixture task even if an assertion fails.
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });

    let mut connection = V2Connection::start_with(
        &["serve", "--backend", "groq"],
        &[
            ("LLM_API_KEY", "fixture-key"),
            ("LLM_BASE_URL", &base_url),
            ("LLM_MODEL", "fixture-model"),
        ],
    )
    .await?;
    let created = connection
        .request("session/new", json!({"cwd": std::env::temp_dir()}))
        .await?;
    let session_id = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("session/new failed")?
        .to_owned();
    // The new session's own idle update follows its response; wait for it so
    // it cannot be mistaken for the end of the turn.
    connection.until_idle(0).await?;
    let mark = connection.notifications.len();
    let accepted = connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"hello"}]}),
        )
        .await?;
    // Admission precedes provider work, so the prompt is accepted first.
    assert!(
        accepted.pointer("/result/messageId").is_some(),
        "{accepted}"
    );
    let turn = connection.until_idle(mark).await?;
    let last = turn.last().ok_or("no updates")?;
    assert_eq!(last["state"], "idle");
    assert_eq!(last["stopReason"], "error", "{last}");
    assert!(
        last.pointer("/error/code").is_some_and(Value::is_i64),
        "{last}"
    );
    assert!(
        connection
            .notifications
            .iter()
            .all(|message| !message.to_string().contains(SENTINEL)),
        "provider detail reached the client"
    );
    connection.stop().await
}
