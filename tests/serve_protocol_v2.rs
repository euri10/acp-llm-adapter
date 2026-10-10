//! Protocol version negotiation on the shipped `serve` binary.
//!
//! Without the `protocol-v2` feature the adapter speaks ACP v1 only, so a
//! client asking for v2 is answered with v1. With it, v1 and v2 connections are
//! routed to separate implementations; v1 is unchanged either way.

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
