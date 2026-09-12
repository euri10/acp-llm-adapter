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

//! Fake ACP agent for proxy child-process tests.
//!
//! Reads NDJSON request lines from stdin, answers one line each on stdout,
//! narrates on stderr, and exits with a configurable status. It is a
//! single-threaded process with sole ownership of its stdio — a test harness
//! would run every test concurrently and race on the very streams under test.
//!
//! This is an example target, not a test target, for two reasons: cargo uplifts
//! example artifacts to `<profile>/examples/<name>` under a stable hash-free
//! name, so the test spawning it names the current build exactly instead of
//! scanning `deps/` and picking a stale hash (daa-30py); and examples may use
//! dev-dependencies, which binaries may not. `cargo test` builds it without
//! running it.

use std::io::{BufRead, Write};

/// Set to make this fixture do anything at all.
const RUN_FIXTURE_ENV: &str = "ACP_LLM_ADAPTER_RUN_PROXY_FIXTURE";
/// Exit code the fixture terminates with.
const FIXTURE_EXIT_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_EXIT";
/// Session id the fixture reports from `session/new`.
const FIXTURE_SESSION_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_SESSION";
/// Set to make the fixture exit immediately, before reading any stdin.
///
/// Simulates a wrapped agent that dies before ever speaking ACP, such as an
/// npm-installed CLI whose postinstall step never ran.
const FIXTURE_EXIT_IMMEDIATELY_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_EXIT_IMMEDIATELY";

/// Default session id, shaped like the ones real agents return.
const DEFAULT_SESSION_ID: &str = "session-fixture-0001";

/// The session id this run reports, overridable to test rejection of bad ids.
fn session_id() -> String {
    std::env::var(FIXTURE_SESSION_ENV).unwrap_or_else(|_| DEFAULT_SESSION_ID.to_string())
}

/// Return the request id if `line` is a `session/new` call.
fn session_new_id(line: &str) -> Option<String> {
    let frame: serde_json::Value = serde_json::from_str(line).ok()?;
    if frame.get("method").and_then(serde_json::Value::as_str)? != "session/new" {
        return None;
    }
    match frame.get("id")? {
        serde_json::Value::String(id) => Some(format!("\"{id}\"")),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::var_os("ACP_PROXY_TREE_ROLE").is_some() {
        if let Err(error) = tree_fixture() {
            eprintln!("tree fixture: {error}");
            std::process::exit(1);
        }
        return;
    }

    if std::env::var_os(RUN_FIXTURE_ENV).is_none() {
        return;
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();

    let _ = writeln!(stderr, "fixture: started");
    let _ = stderr.flush();

    if std::env::var_os(FIXTURE_EXIT_IMMEDIATELY_ENV).is_some() {
        let _ = writeln!(stderr, "fixture: exiting before reading any stdin");
        let _ = stderr.flush();
        std::process::exit(exit_code());
    }

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }

        // Answer session/new the way a real agent does: the id comes back in
        // the result, with no method name to identify it by.
        if let Some(id) = session_new_id(&line) {
            let _ = writeln!(
                stdout,
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"{}"}}}}"#,
                session_id()
            );
            let _ = stdout.flush();
            let _ = writeln!(stderr, "fixture: created a session");
            let _ = stderr.flush();
            continue;
        }

        // Deliberately awkward: a key order no serialiser would choose and a
        // float with a trailing zero. A proxy that reserialised instead of
        // forwarding bytes would silently normalise both.
        let _ = writeln!(
            stdout,
            r#"{{"jsonrpc":"2.0","zebra":1,"alpha":2,"ratio":1.50,"echo":{line}}}"#
        );
        let _ = stdout.flush();
        let _ = writeln!(stderr, "fixture: handled a request");
        let _ = stderr.flush();
    }

    std::process::exit(exit_code());
}

/// Exit code this run should terminate with.
fn exit_code() -> i32 {
    std::env::var(FIXTURE_EXIT_ENV)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0)
}

/// Deliberately uncooperative descendants, including a new Unix session.
#[cfg(target_os = "linux")]
fn tree_fixture() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};

    let role = std::env::var("ACP_PROXY_TREE_ROLE")?;
    let record = |name: &str, pid: u32| -> std::io::Result<()> {
        let path = std::env::var_os("ACP_PROXY_TREE_PIDS")
            .ok_or_else(|| std::io::Error::other("missing pid path"))?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(format!("{name} {pid}\n").as_bytes())
    };
    record(&role, std::process::id())?;
    if role == "client" {
        let proxy = std::env::args_os().nth(1).ok_or("missing proxy")?;
        let mut child = Command::new(proxy)
            .args(["--log-root"])
            .arg(std::env::var_os("ACP_PROXY_TREE_LOGS").ok_or("missing log path")?)
            .arg("--")
            .arg(std::env::current_exe()?)
            .env("ACP_PROXY_TREE_ROLE", "agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        record("proxy", child.id())?;
        let mut stdin = child.stdin.take();
        let mut holder = if std::env::var_os("ACP_PROXY_TREE_HOLD_STDIN").is_some() {
            let process = Command::new("sleep")
                .arg("120")
                .stdin(stdin.take().ok_or("missing proxy stdin")?)
                .spawn()?;
            record("holder", process.id())?;
            Some(process)
        } else {
            None
        };
        // The test kills this client while its pipe remains open.
        child.wait()?;
        if let Some(process) = holder.as_mut() {
            process.kill()?;
            process.wait()?;
        }
    } else if role == "agent" {
        let mut child = Command::new("setsid")
            .arg(std::env::current_exe()?)
            .env("ACP_PROXY_TREE_ROLE", "worker")
            .spawn()?;
        child.wait()?;
    } else if role == "worker" {
        let mut child = Command::new(std::env::current_exe()?)
            .env("ACP_PROXY_TREE_ROLE", "leaf")
            .spawn()?;
        child.wait()?;
    } else {
        // Never read stdin: EOF alone cannot dispose this process tree.
        loop {
            std::thread::park();
        }
    }
    Ok(())
}
