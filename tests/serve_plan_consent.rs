//! Plan exit consent and batch isolation through the shipped ACP server.
#![allow(
    clippy::indexing_slicing,
    reason = "Test assertions index protocol messages and intentionally fail on missing fields."
)]

use std::error::Error;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn provider() -> Result<
    (
        String,
        Arc<Mutex<Vec<Value>>>,
        tokio::task::JoinSet<std::io::Result<()>>,
    ),
    Box<dyn Error>,
> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/models", get(|| async { json!({"data":[{"id":"fixture-model"}]}).to_string() }))
        .route("/chat/completions", post(|State(requests): State<Arc<Mutex<Vec<Value>>>>, body: String| async move {
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if let Ok(mut captured) = requests.lock() {
                captured.push(request.clone());
            }
            let last = request["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
            let text = last["content"].as_str().unwrap_or_default();
            let calls = if last["role"] == "user" && text == "exit-and-write" {
                vec![
                    json!({"index":0,"id":"exit","type":"function","function":{"name":"exit_plan_mode","arguments":"{}"}}),
                    json!({"index":1,"id":"write","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"pwned.txt\",\"content\":\"unauthorized\"}"}}),
                    json!({"index":2,"id":"read-after-exit","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"readable.txt\"}"}}),
                ]
            } else if last["role"] == "user" && text == "write-again" {
                vec![json!({"index":0,"id":"write-again","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"pwned.txt\",\"content\":\"unauthorized\"}"}})]
            } else {
                Vec::new()
            };
            let chunk = if calls.is_empty() {
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
            } else {
                json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]})
            };
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], format!("data: {chunk}\n\ndata: [DONE]\n\n"))
        }))
        .with_state(Arc::clone(&requests));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, app).await });
    Ok((url, requests, server))
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    received: Vec<Value>,
    id: u64,
}

impl Wire {
    fn start(url: &str, root: &std::path::Path) -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(["serve", "--backend", "groq"])
            .env("LLM_API_KEY", "fixture-key")
            .env("LLM_BASE_URL", url)
            .env("LLM_MODEL", "fixture-model")
            .env("XDG_STATE_HOME", root.join("state"))
            .env_remove("ACP_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or("missing stdin")?;
        let lines = BufReader::new(child.stdout.take().ok_or("missing stdout")?).lines();
        Ok(Self {
            child,
            stdin,
            lines,
            received: Vec::new(),
            id: 0,
        })
    }

    async fn send(&mut self, value: &Value) -> Result<(), Box<dyn Error>> {
        self.stdin
            .write_all(format!("{value}\n").as_bytes())
            .await?;
        Ok(())
    }

    async fn next(&mut self) -> Result<Value, Box<dyn Error>> {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await??
            .ok_or("server closed stdout")?;
        let message: Value = serde_json::from_str(&line)?;
        if message.get("method").is_some() {
            self.received.push(message.clone());
        }
        Ok(message)
    }

    async fn begin(&mut self, method: &str, params: Value) -> Result<u64, Box<dyn Error>> {
        self.id += 1;
        let id = self.id;
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
            .await?;
        Ok(id)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.begin(method, params).await?;
        loop {
            let message = self.next().await?;
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                return Ok(message);
            }
        }
    }

    async fn idle(&mut self, from: usize) -> Result<(), Box<dyn Error>> {
        while !self.received[from..]
            .iter()
            .any(|v| v.pointer("/params/update/state") == Some(&json!("idle")))
        {
            self.next().await?;
        }
        Ok(())
    }

    async fn prompt(
        &mut self,
        session: &str,
        text: &str,
        protocol: u8,
        decision: &str,
    ) -> Result<Vec<Value>, Box<dyn Error>> {
        let from = self.received.len();
        let id = self
            .begin(
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
            )
            .await?;
        let mut answered = false;
        loop {
            let message = self.next().await?;
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                self.send(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":decision}}})).await?;
            }
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                assert!(message.get("result").is_some(), "prompt failed: {message}");
                answered = true;
            }
            if answered
                && (protocol == 1
                    || message.pointer("/params/update/state") == Some(&json!("idle")))
            {
                return Ok(self.received[from..].to_vec());
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the wire interaction and its approval and refusal assertions together."
)]
async fn plan_consent(protocol: u8, decision: &str) -> Result<(), Box<dyn Error>> {
    let root =
        Scratch(std::env::temp_dir().join(format!("acp-plan-consent-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&root.0)?;
    std::fs::write(root.0.join("readable.txt"), "available to advertised reads")?;
    let (url, requests, mut server) = provider().await?;
    let mut wire = Wire::start(&url, &root.0)?;
    let init = if protocol == 1 {
        json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}})
    } else {
        json!({"protocolVersion":2,"info":{"name":"plan-test","version":"0.0.0"},"capabilities":{}})
    };
    let initialized = wire.request("initialize", init).await?;
    assert_eq!(
        initialized.pointer("/result/protocolVersion"),
        Some(&json!(protocol))
    );
    let from = wire.received.len();
    let created = wire
        .request("session/new", json!({"cwd":root.0,"mcpServers":[]}))
        .await?;
    let session = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("missing session")?
        .to_owned();
    if protocol == 2 {
        wire.idle(from).await?;
    }
    let mode = if protocol == 1 {
        wire.request(
            "session/set_mode",
            json!({"sessionId":session,"modeId":"plan"}),
        )
        .await?
    } else {
        wire.request(
            "session/set_config_option",
            json!({"sessionId":session,"configId":"mode","type":"id","value":"plan"}),
        )
        .await?
    };
    assert!(mode.get("result").is_some(), "{mode}");

    let updates = wire
        .prompt(&session, "exit-and-write", protocol, decision)
        .await?;
    let advertised = requests
        .lock()
        .map_err(|e| e.to_string())?
        .iter()
        .map(|r| {
            r["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(
        !root.0.join("pwned.txt").exists(),
        "protocol={protocol}, advertised={advertised:?}, calls={updates:?}"
    );
    let permissions: Vec<_> = updates
        .iter()
        .filter(|u| u.get("method") == Some(&json!("session/request_permission")))
        .collect();
    assert_eq!(
        permissions.len(),
        1,
        "protocol={protocol}, advertised={advertised:?}, calls={updates:?}"
    );
    assert!(
        permissions[0]
            .to_string()
            .contains("Approve plan and switch to Accept Edits"),
        "{permissions:?}"
    );
    assert!(
        updates.iter().any(|u| u.to_string().contains("write")
            && u.pointer("/params/update/status") == Some(&json!("failed"))),
        "advertised={advertised:?}, refused calls={updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|u| u.to_string().contains("read-after-exit")
                && u.pointer("/params/update/status") == Some(&json!("failed"))),
        "advertised={advertised:?}, refused calls={updates:?}"
    );

    if decision == "reject_once" {
        let later = wire
            .prompt(&session, "write-again", protocol, "allow_once")
            .await?;
        assert!(
            !root.0.join("pwned.txt").exists(),
            "rejected exit allowed later write; advertised={advertised:?}, calls={later:?}"
        );
        assert!(
            !later
                .iter()
                .any(|u| u.get("method") == Some(&json!("session/request_permission"))),
            "{later:?}"
        );
        assert!(
            later.iter().any(|u| u.to_string().contains("write-again")
                && u.pointer("/params/update/status") == Some(&json!("failed"))),
            "advertised={advertised:?}, refused calls={later:?}"
        );
    }
    drop(wire.stdin);
    assert!(wire.child.wait().await?.success());
    server.shutdown().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn v1_plan_exit_requires_consent_and_cannot_write_in_batch() -> Result<(), Box<dyn Error>> {
    for decision in ["allow_once", "reject_once"] {
        plan_consent(1, decision).await?;
    }
    Ok(())
}

#[cfg(feature = "protocol-v2")]
#[test_log::test(tokio::test)]
async fn v2_plan_exit_requires_consent_and_cannot_write_in_batch() -> Result<(), Box<dyn Error>> {
    for decision in ["allow_once", "reject_once"] {
        plan_consent(2, decision).await?;
    }
    Ok(())
}
