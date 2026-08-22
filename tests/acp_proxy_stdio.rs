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

//! End-to-end tests for the ACP proxy against a scripted fake agent.
//!
//! The fake agent is the harnessless `acp_proxy_fixture` test target, located
//! the same way the MCP stdio fixture is. Keeping it a separate process with
//! sole ownership of its stdio is what makes these assertions meaningful.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use acp_llm_adapter::logsink::{Direction, LogRecord};
use uuid::Uuid;

/// Set to make this binary behave as the fake agent rather than a test runner.
const FIXTURE_ENV: &str = "ACP_LLM_ADAPTER_RUN_PROXY_FIXTURE";
/// Exit code the fake agent should terminate with.
const FIXTURE_EXIT_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_EXIT";
/// Session id the fake agent should report from `session/new`.
const FIXTURE_SESSION_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_SESSION";

/// A `session/new` exchange, as a client would send it.
const SESSION_NEW: &str =
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}}\n";

/// A temp directory that removes itself.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!("acp-proxy-{label}-{}", Uuid::new_v4())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Outcome of driving the proxy over a fake agent.
struct ProxyRun {
    stdout: String,
    exit_code: Option<i32>,
    records: Vec<LogRecord>,
}

/// Run `acp-proxy` wrapping the fixture agent.
fn run_proxy(root: &Path, input: &str, fixture_exit: i32) -> ProxyRun {
    run_proxy_with_session(root, input, fixture_exit, None)
}

/// Run the proxy, optionally dictating the session id the agent reports.
fn run_proxy_with_session(
    root: &Path,
    input: &str,
    fixture_exit: i32,
    session_id: Option<&str>,
) -> ProxyRun {
    let proxy = proxy_binary();
    let fixture = fixture_binary();

    let mut command = Command::new(&proxy);
    command
        .arg("--log-root")
        .arg(root)
        .arg("--")
        .arg(&fixture)
        .env(FIXTURE_ENV, "1")
        .env(FIXTURE_EXIT_ENV, fixture_exit.to_string());
    if let Some(session_id) = session_id {
        command.env(FIXTURE_SESSION_ENV, session_id);
    }

    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => unreachable!("failed to spawn {}: {error}", proxy.display()),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
        // Dropping stdin closes it, which is how a real client signals the end
        // of a session; the proxy must relay that EOF to the wrapped agent.
    }

    let mut stdout = String::new();
    if let Some(mut handle) = child.stdout.take() {
        let _ = handle.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        let _ = handle.read_to_string(&mut stderr);
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => unreachable!("failed to wait for proxy: {error}"),
    };

    ProxyRun {
        stdout,
        exit_code: status.code(),
        records: read_all_records(root),
    }
}

/// Records written under a specific session's directory.
fn session_records(root: &Path, session_id: &str) -> Vec<LogRecord> {
    let mut records = Vec::new();
    collect(&root.join("sessions").join(session_id), &mut records);
    records
}

/// Records written to the connection-scoped files.
fn connection_records(root: &Path) -> Vec<LogRecord> {
    let mut records = Vec::new();
    collect(&root.join("connections"), &mut records);
    records
}

fn proxy_binary() -> PathBuf {
    // Integration tests are built into `deps`; the binaries they exercise sit
    // one level up in the same profile directory.
    let mut path = current_binary();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("acp-proxy")
}

/// Locate the harnessless fixture binary among the compiled test targets.
fn fixture_binary() -> PathBuf {
    let current = current_binary();
    let Some(deps_dir) = current.parent() else {
        unreachable!("test executable has no parent directory")
    };
    let Ok(entries) = std::fs::read_dir(deps_dir) else {
        unreachable!("cannot read {}", deps_dir.display())
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let is_dep_info = path.extension().is_some_and(|extension| extension == "d");
        if file_name.starts_with("acp_proxy_fixture-")
            && !is_dep_info
            && file_name.ends_with(std::env::consts::EXE_SUFFIX)
            && path.is_file()
        {
            return path;
        }
    }

    unreachable!("failed to find the acp_proxy_fixture test executable")
}

fn current_binary() -> PathBuf {
    match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => unreachable!("cannot locate the test binary: {error}"),
    }
}

/// Collect every record written anywhere under the log root.
fn read_all_records(root: &Path) -> Vec<LogRecord> {
    let mut records = Vec::new();
    collect(root, &mut records);
    records
}

fn collect(dir: &Path, records: &mut Vec<LogRecord>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, records);
        } else if let Ok(contents) = std::fs::read_to_string(&path) {
            records.extend(
                contents
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .filter_map(|line| serde_json::from_str::<LogRecord>(line).ok()),
            );
        }
    }
}

fn frames(records: &[LogRecord], direction: Direction) -> Vec<&LogRecord> {
    records
        .iter()
        .filter(|record| record.direction == direction && record.kind == "frame")
        .collect()
}

#[test]
fn forwarded_bytes_reach_the_client_unmodified() {
    let root = TempRoot::new("transparent");

    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 0);

    assert!(
        run.stdout.contains(r#""zebra":1,"alpha":2,"ratio":1.50"#),
        "the agent's own byte sequence must survive the proxy verbatim, got: {}",
        run.stdout
    );
}

#[test]
fn both_directions_are_recorded() {
    let root = TempRoot::new("directions");

    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 0);

    let inbound = frames(&run.records, Direction::ClientToAgent);
    let outbound = frames(&run.records, Direction::AgentToClient);

    assert_eq!(
        inbound.len(),
        1,
        "client-to-agent traffic must be captured; the shell wrapper never was"
    );
    assert_eq!(
        outbound.len(),
        1,
        "agent-to-client traffic must be captured"
    );
    assert_eq!(
        inbound
            .first()
            .and_then(|record| record.payload.get("method"))
            .and_then(serde_json::Value::as_str),
        Some("initialize")
    );
}

#[test]
fn agent_stderr_is_captured_as_text() {
    let root = TempRoot::new("stderr");

    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 0);

    let captured: Vec<_> = run
        .records
        .iter()
        .filter(|record| record.kind == "stderr")
        .filter_map(|record| record.payload.as_str())
        .collect();

    assert!(
        captured
            .iter()
            .any(|line| line.contains("fixture: started")),
        "expected the agent's stderr in the log, got {captured:?}"
    );
}

#[test]
fn a_failing_agent_makes_the_proxy_fail() {
    let root = TempRoot::new("exit");

    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 42);

    assert_eq!(
        run.exit_code,
        Some(42),
        "the wrapped agent's status must reach the client, not the pipeline's"
    );
    assert!(
        run.records.iter().any(|record| record.kind == "exit"
            && record
                .payload
                .get("code")
                .and_then(serde_json::Value::as_i64)
                == Some(42)),
        "the exit status is recorded"
    );
}

#[test]
fn closing_client_stdin_shuts_the_agent_down() {
    let root = TempRoot::new("eof");

    // The fixture only exits when its stdin reaches EOF, so completing at all
    // proves the proxy relayed the client's EOF rather than holding it open.
    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 0);

    assert_eq!(run.exit_code, Some(0));
}

#[test]
fn several_frames_are_logged_individually() {
    let root = TempRoot::new("multi");

    let run = run_proxy(
        root.path(),
        "{\"method\":\"initialize\"}\n{\"method\":\"session/new\"}\n{\"method\":\"session/prompt\"}\n",
        0,
    );

    assert_eq!(frames(&run.records, Direction::ClientToAgent).len(), 3);
    assert_eq!(frames(&run.records, Direction::AgentToClient).len(), 3);
}

#[test]
fn the_launch_invocation_is_recorded() {
    let root = TempRoot::new("launch");

    let run = run_proxy(root.path(), "{\"method\":\"initialize\"}\n", 0);

    assert!(
        run.records.iter().any(|record| record.kind == "launch"),
        "which agent was wrapped is part of what makes a log readable later"
    );
}

// ── session-keyed logs ──────────────────────────────────────

#[test]
fn records_after_session_new_land_under_the_real_session_id() {
    let root = TempRoot::new("session-keyed");

    let run = run_proxy(
        root.path(),
        &format!(
            "{SESSION_NEW}{{\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"session-fixture-0001\"}}}}\n"
        ),
        0,
    );

    assert_eq!(run.exit_code, Some(0));
    let records = session_records(root.path(), "session-fixture-0001");
    assert!(
        !records.is_empty(),
        "the agent's own session id must name the directory; the shell wrapper \
         could never know it, having opened its files before the session existed"
    );
    assert!(
        records
            .iter()
            .all(|record| record.session_id.as_deref() == Some("session-fixture-0001")),
        "every record filed under a session names it"
    );
}

#[test]
fn pre_session_frames_stay_in_the_connection_file() {
    let root = TempRoot::new("pre-session");

    run_proxy(
        root.path(),
        &format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n{SESSION_NEW}"),
        0,
    );

    let connection = connection_records(root.path());
    assert!(
        connection.iter().any(|record| record
            .payload
            .get("method")
            .and_then(serde_json::Value::as_str)
            == Some("initialize")),
        "initialize happens before any session exists, so it has no session to belong to"
    );
}

#[test]
fn the_mapping_from_connection_to_session_is_recorded() {
    let root = TempRoot::new("mapping");

    run_proxy(root.path(), SESSION_NEW, 0);

    let connection = connection_records(root.path());
    assert!(
        connection
            .iter()
            .any(|record| record.kind == "session-bound"
                && record.session_id.as_deref() == Some("session-fixture-0001")),
        "the two halves of a session's story must be stitchable back together"
    );
}

#[test]
fn a_hostile_session_id_cannot_choose_where_we_write() {
    let root = TempRoot::new("hostile");

    let run = run_proxy_with_session(root.path(), SESSION_NEW, 0, Some("../../escape"));

    assert_eq!(run.exit_code, Some(0), "the agent still runs normally");
    assert!(
        !root.path().join("../../escape").exists(),
        "a session id lifted off the wire is untrusted input"
    );
    assert!(
        !connection_records(root.path()).is_empty(),
        "the frames are still logged, just not where the agent asked"
    );
}

#[test]
fn malformed_frames_do_not_stop_the_sniffer() {
    let root = TempRoot::new("malformed");

    let run = run_proxy(root.path(), &format!("not json at all\n{SESSION_NEW}"), 0);

    assert_eq!(run.exit_code, Some(0), "forwarding survives a bad line");
    assert!(
        !session_records(root.path(), "session-fixture-0001").is_empty(),
        "a session created after a malformed line is still detected"
    );
}
