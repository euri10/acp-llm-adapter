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

//! Harnessless fake ACP agent for proxy child-process tests.
//!
//! Reads NDJSON request lines from stdin, answers one line each on stdout,
//! narrates on stderr, and exits with a configurable status. Runs without a
//! test harness so it is a single-threaded process with sole ownership of its
//! stdio — a harnessed binary would run every test concurrently and race on
//! the very streams under test.

use std::io::{BufRead, Write};

/// Set to make this fixture do anything at all.
const RUN_FIXTURE_ENV: &str = "ACP_LLM_ADAPTER_RUN_PROXY_FIXTURE";
/// Exit code the fixture terminates with.
const FIXTURE_EXIT_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_EXIT";
/// Session id the fixture reports from `session/new`.
const FIXTURE_SESSION_ENV: &str = "ACP_LLM_ADAPTER_PROXY_FIXTURE_SESSION";

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
    if std::env::var_os(RUN_FIXTURE_ENV).is_none() {
        return;
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();

    let _ = writeln!(stderr, "fixture: started");
    let _ = stderr.flush();

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

    let code = std::env::var(FIXTURE_EXIT_ENV)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);
    std::process::exit(code);
}
