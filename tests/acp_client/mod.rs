//! A minimal ACP client for driving a real `serve` process over stdio.
//!
//! Integration tests that care about what an editor experiences need to speak
//! the protocol rather than call the functions underneath it, because the
//! interesting failures live in the wiring: a tool call that never reaches the
//! client, a cancel that never reaches the token, a command that outlives the
//! connection. This is the smallest client that can provoke those.

// Two test binaries include this module and each uses a subset of it; without
// this, the unused half is a dead_code error under the crate's deny(warnings).
#![allow(dead_code)]

use std::error::Error;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Why [`Serve::pump`] stopped reading.
#[derive(Debug)]
pub(crate) enum Stopped {
    /// The response to the awaited request arrived.
    Response(Box<Value>),
    /// The caller's predicate became true.
    Predicate,
    /// The deadline passed with neither of the above.
    Timeout,
}

/// A running `acp-llm-adapter serve` process, already through the handshake.
pub(crate) struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    session_id: String,
    /// Every `session/update` notification seen so far, in arrival order.
    ///
    /// Recorded rather than discarded because these are what an editor renders;
    /// a test asserting on the user-visible behaviour of a turn asserts on these.
    pub(crate) notifications: Vec<Value>,
    next_id: u64,
}

impl Serve {
    /// Spawn `serve --backend mock`, initialize, and open a session.
    ///
    /// Declares no terminal capability, so the adapter runs commands itself
    /// instead of handing them back to the client.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be spawned or the handshake does
    /// not complete.
    pub(crate) async fn start() -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(["serve", "--backend", "mock"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or("serve exposed no stdin")?;
        let stdout = child.stdout.take().ok_or("serve exposed no stdout")?;

        let mut serve = Self {
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            session_id: String::new(),
            notifications: Vec::new(),
            next_id: 1,
        };

        let initialize = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            }
        });
        serve.request("initialize", &initialize).await?;

        let new_session = json!({"cwd": "/tmp", "mcpServers": []});
        let session = serve.request("session/new", &new_session).await?;
        session
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .ok_or("session/new returned no session id")?
            .clone_into(&mut serve.session_id);
        Ok(serve)
    }

    /// The session opened during [`Serve::start`].
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a request and read until its response arrives.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails or no response arrives in time.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match self
            .pump(Duration::from_secs(10), Some(id), || false)
            .await?
        {
            Stopped::Response(response) => Ok(*response),
            other => Err(format!("no response to {method}: {other:?}").into()),
        }
    }

    /// Send a notification, which has no response.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub(crate) async fn notify(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<(), Box<dyn Error>> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Start a prompt turn, returning the request id to await later.
    ///
    /// Does not wait for the turn: callers that cancel or disconnect mid-turn
    /// need to act while it is still running.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub(crate) async fn start_prompt(&mut self, text: &str) -> Result<u64, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}]
        });
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params}),
        )
        .await?;
        Ok(id)
    }

    /// Read messages until the response to `until_id` arrives, `until` becomes
    /// true, or `limit` elapses.
    ///
    /// Permission requests are granted as they arrive, and every `session/update`
    /// notification is recorded, so callers never have to interleave that
    /// bookkeeping with what they are actually waiting for.
    ///
    /// # Errors
    ///
    /// Returns an error if the process closes stdout or a write fails.
    pub(crate) async fn pump(
        &mut self,
        limit: Duration,
        until_id: Option<u64>,
        mut until: impl FnMut() -> bool,
    ) -> Result<Stopped, Box<dyn Error>> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if until() {
                return Ok(Stopped::Predicate);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Stopped::Timeout);
            }
            let line = tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(20)) => continue,
                line = self.lines.next_line() => line?,
            };
            let Some(line) = line else {
                return Err("serve closed stdout".into());
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                self.notifications.push(message);
                continue;
            }
            if let Some(reply) = permission_grant(&message) {
                self.send(&reply).await?;
                continue;
            }
            if message.get("method").is_none()
                && until_id.is_some_and(|id| message.get("id").and_then(Value::as_u64) == Some(id))
            {
                return Ok(Stopped::Response(Box::new(message)));
            }
        }
    }

    /// The `update` payloads of every recorded notification of one kind.
    ///
    /// Selecting by kind rather than by position keeps tests from breaking when
    /// the adapter adds an unrelated update, which it is free to do.
    pub(crate) fn updates(&self, kind: &str) -> Vec<&Value> {
        self.notifications
            .iter()
            .filter_map(|notification| notification.pointer("/params/update"))
            .filter(|update| update.get("sessionUpdate").and_then(Value::as_str) == Some(kind))
            .collect()
    }

    /// Close stdin, as an editor does when it exits.
    pub(crate) fn disconnect(&mut self) {
        self.stdin = None;
    }

    /// Wait for the process to exit.
    ///
    /// # Errors
    ///
    /// Returns an error if it does not exit within `limit`.
    pub(crate) async fn wait(
        &mut self,
        limit: Duration,
    ) -> Result<std::process::ExitStatus, Box<dyn Error>> {
        tokio::time::timeout(limit, self.child.wait())
            .await?
            .map_err(Into::into)
    }

    async fn send(&mut self, message: &Value) -> Result<(), Box<dyn Error>> {
        let stdin = self.stdin.as_mut().ok_or("client already disconnected")?;
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
        Ok(())
    }
}

/// Build an allow reply if `message` is a permission request.
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
            "outcome": {"outcome": "selected", "optionId": option.get("optionId")?}
        }
    }))
}

/// Whether `pid` names a process that has not exited.
///
/// Reads the state field: an unreaped process keeps its `/proc` directory, so
/// existence alone reports a killed descendant as alive (daa-vh77).
#[cfg(target_os = "linux")]
pub(crate) fn alive(pid: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.split_whitespace().next(), None | Some("Z"))
}

/// A shell command that backgrounds a long sleep and records its pid.
///
/// Killing only the shell leaves that sleep behind, which is what makes it a
/// usable probe for whether a whole process group was disposed of.
pub(crate) fn backgrounding_command(pid_file: &std::path::Path) -> String {
    format!(
        "/bin/sleep 300 & echo $! > {}; wait",
        pid_file.to_string_lossy()
    )
}
