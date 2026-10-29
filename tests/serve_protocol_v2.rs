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

#[cfg(feature = "protocol-v2")]
#[path = "serve_protocol_v2/promotion.rs"]
mod promotion;

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
    // Advertise only the implemented transports and session extensions.
    assert_eq!(
        at("/capabilities"),
        Some(json!({"session": {"mcp": {"stdio": {}, "http": {}}, "additionalDirectories": {}}})),
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
    /// Requests the agent sent us (e.g. permission prompts), unanswered.
    incoming: Vec<Value>,
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
            incoming: Vec::new(),
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
        } else if message.get("method").is_some() && message.get("id").is_some() {
            self.incoming.push(message.clone());
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

    /// Wait for the agent to send a request with `method`, and take it.
    async fn wait_request(&mut self, method: &str) -> Result<Value, Box<dyn Error>> {
        loop {
            if let Some(index) = self
                .incoming
                .iter()
                .position(|request| request["method"] == method)
            {
                return Ok(self.incoming.remove(index));
            }
            self.next_message().await?;
        }
    }

    async fn respond(&mut self, request: &Value, result: Value) -> Result<(), Box<dyn Error>> {
        let response = json!({"jsonrpc":"2.0", "id": request["id"], "result": result});
        self.stdin
            .write_all(format!("{response}\n").as_bytes())
            .await?;
        Ok(())
    }

    /// Send a request without waiting for its response; returns its id.
    async fn send(&mut self, method: &str, params: Value) -> Result<u64, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        Ok(id)
    }

    async fn new_session(
        &mut self,
        cwd: &std::path::Path,
    ) -> Result<(String, Value), Box<dyn Error>> {
        let created = self.request("session/new", json!({"cwd": cwd})).await?;
        let session_id = created
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("session/new failed: {created}"))?
            .to_owned();
        let mark = self.notifications.len();
        self.until_idle(mark).await?;
        Ok((session_id, created))
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
    assert_eq!(
        kinds(&replayed),
        ["user_message", "agent_message", "state_update:idle"]
    );
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
            json!({"cwd": cwd, "mcpServers": [{"type":"sse","name":"x","url":"http://127.0.0.1:1"}]}),
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

#[cfg(feature = "protocol-v2")]
fn tool_updates(updates: &[Value]) -> Vec<&Value> {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "tool_call_update")
        .collect()
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_config_options_expose_modes_and_plan_refuses_commands() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    let (session_id, created) = connection.new_session(&std::env::temp_dir()).await?;
    let mode = |options: &Value| {
        options
            .as_array()
            .and_then(|options| options.iter().find(|option| option["configId"] == "mode"))
            .map(|option| option["currentValue"].clone())
    };
    assert_eq!(
        mode(&created["result"]["configOptions"]),
        Some(json!("ask")),
        "{created}"
    );

    let set = connection
        .request(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "mode", "type": "id", "value": "plan"}),
        )
        .await?;
    assert_eq!(
        mode(&set["result"]["configOptions"]),
        Some(json!("plan")),
        "{set}"
    );

    // Plan refuses execution before any permission prompt.
    let mark = connection.notifications.len();
    connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"!tool run_command echo planned"}]}),
        )
        .await?;
    let turn = connection.until_idle(mark).await?;
    assert!(
        connection.incoming.is_empty(),
        "plan mode asked for permission"
    );
    assert!(
        tool_updates(&turn)
            .iter()
            .any(|update| update["status"] == "failed"),
        "{turn:?}"
    );
    connection.stop().await
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_runs_gated_commands_and_confined_reads_locally() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::temp_dir().join(format!("acp-v2-tools-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd)?;
    std::fs::write(cwd.join("notes.txt"), "local notes\n")?;
    let mut connection = V2Connection::start().await?;
    let (session_id, _) = connection.new_session(&cwd).await?;

    // read_file runs locally, confined to the session directory, unprompted.
    let mark = connection.notifications.len();
    connection
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"!tool read_file notes.txt"}]}),
        )
        .await?;
    let turn = connection.until_idle(mark).await?;
    assert!(
        connection.incoming.is_empty(),
        "read_file asked for permission"
    );
    assert!(
        tool_updates(&turn)
            .iter()
            .any(|update| update["status"] == "completed"
                && update.to_string().contains("local notes")),
        "{turn:?}"
    );

    // run_command waits for a v2 permission request about the tool call.
    let mark = connection.notifications.len();
    connection
        .send(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"!tool run_command echo v2-ran"}]}),
        )
        .await?;
    let permission = connection
        .wait_request("session/request_permission")
        .await?;
    assert_eq!(permission["params"]["sessionId"], json!(session_id));
    assert_eq!(
        permission["params"]["subject"]["type"], "command",
        "{permission}"
    );
    assert_eq!(
        permission["params"]["subject"]["toolCallId"],
        "mock-tool-call-0"
    );
    assert_eq!(permission["params"]["subject"]["command"], "echo v2-ran");
    assert_eq!(permission["params"]["subject"]["cwd"], json!(cwd));
    assert_eq!(
        connection
            .notifications
            .last()
            .and_then(|n| n.pointer("/params/update/state")),
        Some(&json!("requires_action"))
    );
    let allow = permission["params"]["options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["kind"] == "allow_once"))
        .ok_or("no allow_once option")?["optionId"]
        .clone();
    connection
        .respond(
            &permission,
            json!({"outcome": {"outcome": "selected", "optionId": allow}}),
        )
        .await?;
    let turn = connection.until_idle(mark).await?;
    let states: Vec<_> = turn
        .iter()
        .filter_map(|n| n.get("state"))
        .cloned()
        .collect();
    assert_eq!(
        states,
        vec![
            json!("running"),
            json!("requires_action"),
            json!("running"),
            json!("idle")
        ]
    );
    assert!(
        tool_updates(&turn)
            .iter()
            .any(|update| update["status"] == "in_progress"),
        "{turn:?}"
    );
    assert!(
        tool_updates(&turn)
            .iter()
            .any(|update| update["status"] == "completed" && update.to_string().contains("v2-ran")),
        "{turn:?}"
    );
    connection.stop().await?;
    std::fs::remove_dir_all(cwd)?;
    Ok(())
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_switching_to_plan_revokes_a_pending_approval() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::temp_dir().join(format!("acp-v2-revoke-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd)?;
    let marker = cwd.join("must-not-exist");
    let mut connection = V2Connection::start().await?;
    let (session_id, _) = connection.new_session(&cwd).await?;
    let mark = connection.notifications.len();
    connection
        .send(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [
                {"type":"text","text": format!("!tool run_command touch {}", marker.display())}
            ]}),
        )
        .await?;
    let permission = connection
        .wait_request("session/request_permission")
        .await?;
    let set = connection
        .request(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "mode", "type": "id", "value": "plan"}),
        )
        .await?;
    assert!(set.get("result").is_some(), "{set}");
    let allow = permission["params"]["options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["kind"] == "allow_once"))
        .ok_or("no allow_once option")?["optionId"]
        .clone();
    connection
        .respond(
            &permission,
            json!({"outcome": {"outcome": "selected", "optionId": allow}}),
        )
        .await?;
    let turn = connection.until_idle(mark).await?;
    assert!(
        !marker.exists(),
        "an approval granted before Plan ran the command"
    );
    assert!(
        tool_updates(&turn)
            .iter()
            .any(|update| update["status"] == "failed"),
        "{turn:?}"
    );
    connection.stop().await?;
    std::fs::remove_dir_all(cwd)?;
    Ok(())
}

#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn v2_clear_command_empties_the_history() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    let cwd = std::env::temp_dir();
    let (session_id, _) = connection.new_session(&cwd).await?;
    let prompt =
        |text: &str| json!({"sessionId": session_id, "prompt": [{"type":"text","text":text}]});
    let mark = connection.notifications.len();
    connection
        .request("session/prompt", prompt("hello"))
        .await?;
    connection.until_idle(mark).await?;

    let mark = connection.notifications.len();
    let cleared = connection
        .request("session/prompt", prompt("/clear"))
        .await?;
    assert!(cleared.pointer("/result/messageId").is_some(), "{cleared}");
    let turn = connection.until_idle(mark).await?;
    assert!(
        turn.iter()
            .any(|update| update["content"]["text"] == "Conversation history cleared."),
        "{turn:?}"
    );
    assert_eq!(
        turn.last().map(|update| update["stopReason"].clone()),
        Some(json!("end_turn"))
    );

    let mark = connection.notifications.len();
    connection
        .request(
            "session/resume",
            json!({"sessionId": session_id, "cwd": cwd, "replayFrom": {"type": "start"}}),
        )
        .await?;
    assert!(
        connection.notifications[mark..]
            .iter()
            .all(
                |notification| notification.pointer("/params/update/sessionUpdate")
                    == Some(&json!("state_update"))
            ),
        "cleared history was replayed"
    );
    connection.stop().await
}

#[cfg(feature = "protocol-v2")]
/// Updates for one prompt: everything after `running` up to its `idle`.
async fn sdk_until_idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<
        agent_client_protocol::schema::v2::UpdateSessionNotification,
    >,
    session_id: &agent_client_protocol::schema::v2::SessionId,
) -> Result<
    (
        Vec<agent_client_protocol::schema::v2::SessionUpdate>,
        Option<agent_client_protocol::schema::v2::StopReason>,
    ),
    agent_client_protocol::Error,
> {
    use agent_client_protocol::schema::v2;
    let mut running = false;
    let mut seen = Vec::new();
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(10), updates.recv())
            .await
            .map_err(agent_client_protocol::Error::into_internal_error)?
            .ok_or_else(agent_client_protocol::Error::internal_error)?;
        if &notification.session_id != session_id {
            continue;
        }
        match notification.update {
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => running = true,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) if running => {
                return Ok((seen, idle.stop_reason));
            }
            update if running => seen.push(update),
            _ => {}
        }
    }
}

#[cfg(feature = "protocol-v2")]
/// The test client's permission policy: approve once.
fn allow_once(
    request: &agent_client_protocol::schema::v2::RequestPermissionRequest,
) -> Result<
    agent_client_protocol::schema::v2::RequestPermissionResponse,
    agent_client_protocol::Error,
> {
    use agent_client_protocol::schema::v2;
    let allow = request
        .options
        .iter()
        .find(|option| option.kind == v2::PermissionOptionKind::AllowOnce)
        .map(|option| option.option_id.clone())
        .ok_or_else(agent_client_protocol::Error::internal_error)?;
    Ok(v2::RequestPermissionResponse::new(
        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(allow)),
    ))
}

/// Drive the shipped `serve` with the official SDK v2 client: the strongest
/// interoperability evidence available while no editor speaks v2.
#[cfg(feature = "protocol-v2")]
#[tokio::test]
async fn the_sdk_v2_client_drives_a_session_end_to_end() -> Result<(), Box<dyn Error>> {
    use agent_client_protocol::schema::v2;
    use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, V2ConnectionTo};
    use tokio::sync::mpsc::unbounded_channel;

    let state_dir = std::env::temp_dir().join(format!("acp-v2-sdk-{}", uuid::Uuid::new_v4()));
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(["serve", "--backend", "mock"])
            .env("XDG_STATE_HOME", state_dir.display().to_string()),
    );
    let (update_tx, mut updates) = unbounded_channel();
    Client
        .v2()
        .name("serve-protocol-v2-sdk-test")
        .on_receive_notification(
            async move |notification: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .send(notification)
                    .map_err(agent_client_protocol::Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: agent_client_protocol::Responder<
                v2::RequestPermissionResponse,
            >,
                        _connection: V2ConnectionTo<Agent>| {
                responder.respond(allow_once(&request)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection| {
            let initialized = connection
                .send_request(v2::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V2,
                    v2::Implementation::new("serve-protocol-v2-sdk-test", "0.0.0"),
                ))
                .block_task()
                .await?;
            assert_eq!(
                initialized.protocol_version,
                agent_client_protocol::schema::ProtocolVersion::V2
            );
            assert!(initialized.capabilities.session.is_some());

            let session = connection
                .build_session(std::env::temp_dir())
                .start_session()
                .block_task()
                .await?
                .into_session();

            // A permission-gated command, approved by the client's handler.
            session
                .send_prompt("!tool run_command echo sdk-client-ran")
                .block_task()
                .await?;
            let (turn, stop) = sdk_until_idle(&mut updates, session.session_id()).await?;
            assert_eq!(stop, Some(v2::StopReason::EndTurn));
            assert!(
                turn.iter().any(|update| matches!(
                    update,
                    v2::SessionUpdate::ToolCallUpdate(call)
                        if serde_json::to_string(call).is_ok_and(|json|
                            json.contains("\"completed\"") && json.contains("sdk-client-ran"))
                )),
                "{turn:?}"
            );

            // A plain prompt streams the model's answer.
            session.send_prompt("hello").block_task().await?;
            let (turn, stop) = sdk_until_idle(&mut updates, session.session_id()).await?;
            assert_eq!(stop, Some(v2::StopReason::EndTurn));
            assert!(turn.iter().any(|update| matches!(
                update,
                v2::SessionUpdate::AgentMessageChunk(chunk)
                    if matches!(&chunk.content, v2::ContentBlock::Text(text)
                        if text.text == "mock response to: hello")
            )));

            session.close().block_task().await?;
            Ok(())
        })
        .await?;
    if state_dir.exists() {
        std::fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}
