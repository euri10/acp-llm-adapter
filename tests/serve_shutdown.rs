#![forbid(unsafe_code)]
#![deny(
    warnings,
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

//! Integration test for the serve shutdown path.
//!
//! Regression guard for dangling `acp-llm-adapter serve` processes: when
//! the ACP client closes stdin, the adapter must exit instead of hanging
//! forever. The test spawns the real binary, closes its stdin, and asserts the
//! process exits within a short timeout.

use std::error::Error;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Closing the child's stdin must make `serve` exit promptly.
///
/// Uses the `mock` backend so no `LLM_API_KEY` is required to reach the
/// serve loop. If the dangling-process hang ever regresses, the `timeout`
/// elapses and the test fails instead of blocking the suite.
///
/// # Errors
///
/// Returns an error if the binary cannot be spawned, the wait operation fails, or the timeout elapses.
///
/// # Panics
///
/// Panics if the child exits with a non-zero status.
#[test_log::test(tokio::test)]
async fn serve_exits_when_stdin_closes() -> Result<(), Box<dyn Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .args(["serve", "--backend", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    // Drop the write end of the child's stdin → EOF, which must trigger shutdown.
    drop(child.stdin.take());

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    assert!(status.success(), "serve exited unsuccessfully: {status:?}");
    Ok(())
}

/// Whether `pid` names a process that has not exited.
///
/// Reads the state field: an unreaped process keeps its `/proc` directory, so
/// existence alone would report a killed descendant as alive (daa-vh77).
#[cfg(target_os = "linux")]
fn alive(pid: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.split_whitespace().next(), None | Some("Z"))
}

/// A disconnecting client must take a running command's descendants with it.
///
/// The half of daa-zsjw that could not be verified until the mock backend could
/// emit a tool call (daa-wu5e): everything below drives the real binary over
/// real ACP, so it covers the path an editor drives rather than the function in
/// isolation — the tool call, the permission round-trip, and the shutdown.
///
/// Linux-only because it reads `/proc` for the descendant's state; the
/// behaviour it guards applies to every unix.
///
/// # Errors
///
/// Returns an error if the handshake, the tool call, or the shutdown does not
/// complete within its timeout.
#[cfg(target_os = "linux")]
#[test_log::test(tokio::test)]
async fn serve_shutdown_takes_a_running_commands_descendants_with_it() -> Result<(), Box<dyn Error>>
{
    let pid_file = std::env::temp_dir().join(format!(
        "acp-serve-shutdown-descendant-{}.pid",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_file);

    let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .args(["serve", "--backend", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
    let stdout = child.stdout.take().ok_or("serve exposed no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    // No terminal capability, so the adapter runs the command itself rather
    // than handing it to us — the branch this is about.
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            }
        }
    });
    send(&mut stdin, &initialize).await?;
    response(&mut lines, 1).await?;

    let new_session = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": "/tmp", "mcpServers": []}
    });
    send(&mut stdin, &new_session).await?;
    let session = response(&mut lines, 2).await?;
    let session_id = session
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("session/new returned no session id")?
        .to_owned();

    // A command that backgrounds work and waits: killing the shell alone would
    // leave the sleep behind.
    let command = format!(
        "/bin/sleep 300 & echo $! > {}; wait",
        pid_file.to_string_lossy()
    );
    let prompt = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": format!("!tool run_command {command}")}]
        }
    });
    send(&mut stdin, &prompt).await?;

    // Grant permission when asked, and wait for the command to prove it started
    // by recording its descendant.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let descendant = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file) {
            let pid = contents.trim().to_owned();
            if !pid.is_empty() {
                break pid;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("the command never started".into());
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
            line = lines.next_line() => {
                let Some(line) = line? else {
                    return Err("serve closed stdout before running the command".into());
                };
                if let Ok(message) = serde_json::from_str::<Value>(&line)
                    && let Some(reply) = permission_grant(&message)
                {
                    send(&mut stdin, &reply).await?;
                }
            }
        }
    };
    assert!(
        alive(&descendant),
        "the descendant was gone before the client disconnected"
    );

    // Disconnect mid-command, exactly as closing an editor does.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await??;
    assert!(status.success(), "serve exited unsuccessfully: {status:?}");

    let gone_by = tokio::time::Instant::now() + Duration::from_secs(5);
    while alive(&descendant) {
        if tokio::time::Instant::now() >= gone_by {
            let _ = std::fs::remove_file(&pid_file);
            return Err(format!("descendant {descendant} outlived the disconnected client").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}

/// Write one JSON-RPC message as a line.
#[cfg(target_os = "linux")]
async fn send(
    stdin: &mut tokio::process::ChildStdin,
    message: &Value,
) -> Result<(), Box<dyn Error>> {
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .map_err(Into::into)
}

/// Read until the response to `id`, skipping notifications and requests.
#[cfg(target_os = "linux")]
async fn response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Result<Value, Box<dyn Error>> {
    let read = async {
        while let Some(line) = lines.next_line().await? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id)
            {
                return Ok(message);
            }
        }
        Err::<Value, Box<dyn Error>>("serve closed stdout during the handshake".into())
    };
    tokio::time::timeout(Duration::from_secs(10), read).await?
}

/// Build an allow reply if `message` is a permission request.
#[cfg(target_os = "linux")]
fn permission_grant(message: &Value) -> Option<Value> {
    if message.get("method").and_then(Value::as_str)? != "session/request_permission" {
        return None;
    }
    let options = message.pointer("/params/options")?.as_array()?;
    let option = options
        .iter()
        .find(|option| {
            option
                .get("optionId")
                .and_then(Value::as_str)
                .is_some_and(|id| id.contains("allow"))
        })
        .or_else(|| options.first())?;
    Some(json!({
        "jsonrpc": "2.0",
        "id": message.get("id")?,
        "result": {
            "outcome": {
                "outcome": "selected",
                "optionId": option.get("optionId")?
            }
        }
    }))
}
