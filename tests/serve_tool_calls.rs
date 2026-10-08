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

//! What an editor sees while the adapter runs a tool call.
//!
//! Everything here drives a real `serve` process over real ACP. The functions
//! underneath are covered by unit tests; what those cannot see is the wiring —
//! whether the notifications an editor renders actually arrive, and whether an
//! inbound `session/cancel` actually reaches the token the tool is watching. A
//! break anywhere in that wiring leaves every unit test passing.

mod acp_client;

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::routing::{get, post};
use futures_util::{StreamExt as _, stream};
use serde_json::{Value, json};

use acp_client::{Serve, Stopped, alive, backgrounding_command};

struct LogRoot(PathBuf);

impl Drop for LogRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test_log::test(tokio::test)]
async fn serve_redacts_command_content_in_every_log_but_preserves_the_editor_payload()
-> Result<(), Box<dyn Error>> {
    let secret = "ACP_LOG_SECRET_SENTINEL";
    for unredacted in [None, Some("0"), Some("1")] {
        let root = LogRoot(
            std::env::temp_dir().join(format!("acp-serve-redaction-{}", uuid::Uuid::new_v4())),
        );
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        command
            .args(["serve", "--backend", "mock"])
            .env("XDG_STATE_HOME", &root.0)
            .env("ACP_LOG", "1")
            .env("RUST_LOG", "acp_llm_adapter=trace")
            .env_remove("ACP_LOG_MAX_BYTES")
            .env_remove("ACP_LOG_MAX_AGE_DAYS")
            .env_remove("ACP_LOG_UNREDACTED");
        if let Some(unredacted) = unredacted {
            command.env("ACP_LOG_UNREDACTED", unredacted);
        }
        let mut serve =
            Serve::start_with(command, json!({"cwd": "/tmp", "mcpServers": []})).await?;
        let response =
            run_prompt(&mut serve, &format!("!tool run_command printf {secret}")).await?;
        assert_eq!(
            response.pointer("/result/stopReason"),
            Some(&json!("end_turn"))
        );
        let output = serve.updates("tool_call_update");
        assert!(
            output
                .last()
                .is_some_and(|update| update.to_string().contains(secret)),
            "output never reached the editor"
        );
        let session_id = serve.session_id().to_owned();
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(10)).await?.success());
        let log_root = root.0.join("acp-llm-adapter");
        let session =
            std::fs::read_to_string(log_root.join("sessions").join(session_id).join("log.jsonl"))?;
        let mut logs = session.clone();
        for entry in std::fs::read_dir(log_root.join("connections"))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                logs.push_str(&std::fs::read_to_string(path)?);
            }
        }
        assert_eq!(
            logs.contains(secret),
            unredacted == Some("1"),
            "incorrect persisted redaction policy"
        );
        for method in ["session/update", "session/request_permission"] {
            assert!(
                session.contains(method),
                "missing logged protocol stage {method}"
            );
        }
        assert!(logs.contains("trace-event"), "tracing was not exercised");
    }
    Ok(())
}

/// Run one prompt to completion and return the client's view of it.
async fn run_prompt(serve: &mut Serve, text: &str) -> Result<Value, Box<dyn Error>> {
    let id = serve.start_prompt(text).await?;
    match serve
        .pump(Duration::from_secs(20), Some(id), || false)
        .await?
    {
        Stopped::Response(response) => Ok(*response),
        other => Err(format!("prompt never finished: {other:?}").into()),
    }
}

#[test_log::test(tokio::test)]
async fn cancellation_during_builtin_approval_prevents_execution_and_allows_recovery()
-> Result<(), Box<dyn Error>> {
    let root =
        LogRoot(std::env::temp_dir().join(format!("acp-cancel-approval-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&root.0)?;
    let marker = root.0.join("executed");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "mock", "--max-turn-requests", "1"])
        .env("XDG_STATE_HOME", &root.0)
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd": root.0, "mcpServers": []})).await?;
    let id = serve
        .start_prompt("!tool run_command printf executed > executed")
        .await?;
    let pending = serve
        .pump_with_permission(Duration::from_secs(5), Some(id), || false, None)
        .await?;
    let Stopped::Permission(permission) = pending else {
        return Err(format!("expected built-in approval, got {pending:?}").into());
    };
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    let result = serve
        .pump_with_permission(Duration::from_secs(2), Some(id), || false, None)
        .await?;
    assert!(
        matches!(&result, Stopped::Response(response)
            if response.pointer("/result/stopReason") == Some(&json!("cancelled"))),
        "cancel must finish without an approval reply, got {result:?}"
    );
    assert!(serve.position_of_status("in_progress").is_none());
    serve
        .select_permission(
            permission.get("id").ok_or("missing approval id")?,
            "allow_once",
        )
        .await?;
    let response = run_prompt(&mut serve, "hello after cancellation").await?;
    assert_eq!(
        response.pointer("/result/stopReason"),
        Some(&json!("end_turn"))
    );
    assert!(
        !marker.exists(),
        "late approval executed a cancelled command"
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

fn tool_index_provider(posts: Arc<AtomicUsize>) -> Router {
    Router::new()
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
            post(|State(posts): State<Arc<AtomicUsize>>| async move {
                let call = json!({"index":0, "id":"call-0", "function":{
                "name":"run_command", "arguments":"{\"command\":\"printf TOOL_INDEX_OK\"}"}});
                let attempt = posts.fetch_add(1, Ordering::SeqCst);
                let chunks = match attempt {
                    // Small enough to run safely even if validation regresses. Include
                    // a complete valid call to prove the whole batch is rejected.
                    0 => vec![json!({"choices":[{"delta":{"tool_calls":[call,
                    {"index":128, "id":"bad-index", "function":{
                        "name":"run_command", "arguments":"{}"}}]},
                    "finish_reason":null}]})],
                    1 => vec![
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,
                        "id":"call-0", "function":{"name":"run_command",
                            "arguments":"{\"command\":"}}]}, "finish_reason":null}]}),
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,
                        "function":{"arguments":"\"printf TOOL_INDEX_OK\"}"}}]},
                        "finish_reason":"tool_calls"}]}),
                    ],
                    _ => vec![json!({"choices":[{"delta":{"content":"done"},
                    "finish_reason":"stop"}]})],
                };
                let mut body = String::new();
                for chunk in chunks {
                    body.push_str("data: ");
                    body.push_str(&chunk.to_string());
                    body.push_str("\n\n");
                }
                let body = if attempt == 0 {
                    // Keep the invalid response open: rejection must happen on the
                    // delta itself, without waiting for completion or disconnect.
                    Body::from_stream(
                        stream::once(async move { Ok::<_, std::io::Error>(body) })
                            .chain(stream::pending()),
                    )
                } else {
                    body.push_str("data: [DONE]\n\n");
                    Body::from(body)
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        )
        .with_state(posts)
}

#[test_log::test(tokio::test)]
async fn invalid_tool_index_fails_before_execution_and_session_recovers()
-> Result<(), Box<dyn Error>> {
    let posts = Arc::new(AtomicUsize::new(0));
    let router = tool_index_provider(Arc::clone(&posts));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    let root =
        LogRoot(std::env::temp_dir().join(format!("acp-tool-index-{}", uuid::Uuid::new_v4())));
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "groq"])
        .env("LLM_API_KEY", "fixture-key")
        .env("LLM_BASE_URL", base_url)
        .env("LLM_MODEL", "fixture-model")
        .env("XDG_STATE_HOME", &root.0)
        .env_remove("ACP_LOG");
    let mut serve = Serve::start_with(command, json!({"cwd":"/tmp", "mcpServers":[]})).await?;
    let failed = run_prompt(&mut serve, "invalid batch").await?;
    assert_eq!(failed.pointer("/error/code"), Some(&json!(-32603)));
    assert_eq!(
        failed.pointer("/error/data"),
        Some(&json!("provider returned an invalid response"))
    );
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "invalid completion was retried"
    );
    assert!(
        serve
            .position_of_method("session/request_permission")
            .is_none()
    );
    assert!(serve.updates("tool_call").is_empty());
    assert!(serve.updates("tool_call_update").is_empty());
    let history = std::fs::read_to_string(
        root.0
            .join("acp-llm-adapter/sessions")
            .join(serve.session_id())
            .join("history.jsonl"),
    )?;
    for line in history.lines() {
        let message: Value = serde_json::from_str(line)?;
        assert_eq!(
            message.get("role"),
            Some(&json!("user")),
            "invalid provider batch changed conversation history"
        );
    }

    let recovered = run_prompt(&mut serve, "valid fragmented call").await?;
    assert_eq!(
        recovered.pointer("/result/stopReason"),
        Some(&json!("end_turn"))
    );
    assert_eq!(posts.load(Ordering::SeqCst), 3);
    assert!(
        serve
            .position_of_method("session/request_permission")
            .is_some()
    );
    assert_eq!(serve.updates("tool_call").len(), 1);
    assert!(
        serve
            .updates("tool_call_update")
            .iter()
            .any(|update| update.get("status") == Some(&json!("completed"))
                && update.to_string().contains("TOOL_INDEX_OK")),
        "valid tool deltas did not execute after rejection"
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    server.shutdown().await;
    Ok(())
}

/// The editor must be told a tool ran, and told what it produced.
///
/// These notifications are the whole UI for a tool call. Without them an editor
/// showing a stuck spinner and one showing real command output are the same
/// program as far as the adapter's own unit tests are concerned.
///
/// # Errors
///
/// Returns an error if the turn does not complete in time.
///
/// # Panics
///
/// Panics if the tool call, its terminal status, or its output never reach the
/// client.
#[test_log::test(tokio::test)]
async fn a_tool_call_and_its_output_reach_the_client() -> Result<(), Box<dyn Error>> {
    let mut serve = Serve::start().await?;
    let response = run_prompt(&mut serve, "!tool run_command echo hello").await?;

    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("end_turn"),
        "unexpected turn outcome: {response}"
    );

    // Announced before it runs, with what the editor needs to render a row.
    let calls = serve.updates("tool_call");
    let [call] = calls.as_slice() else {
        return Err(format!("expected exactly one tool_call, got {}", calls.len()).into());
    };
    assert_eq!(call.get("kind").and_then(Value::as_str), Some("execute"));
    assert_eq!(
        call.get("title").and_then(Value::as_str),
        Some("echo hello")
    );
    let call_id = call
        .get("toolCallId")
        .and_then(Value::as_str)
        .ok_or("tool_call carried no toolCallId")?;

    // Resolved afterwards, against the same id, carrying the output. Takes the
    // terminal update rather than requiring a single one: the adapter is free
    // to report progress in between, and does.
    let updates = serve.updates("tool_call_update");
    let update = updates
        .last()
        .ok_or("no tool_call_update reached the client")?;
    assert_eq!(
        update.get("toolCallId").and_then(Value::as_str),
        Some(call_id),
        "the update did not resolve the call the client was shown"
    );
    assert_eq!(
        update.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        update.to_string().contains("hello"),
        "the command's output never reached the client: {update}"
    );
    Ok(())
}

/// A running command must be distinguishable from one awaiting approval.
///
/// Announced as pending, the call stays that way across the permission
/// round-trip, so the editor cannot tell "waiting for you" from "working". The
/// `in_progress` update must arrive only once the command is actually running,
/// which is why it is reported from inside the tool rather than from the turn
/// loop (daa-reep).
///
/// # Errors
///
/// Returns an error if the turn does not complete in time.
///
/// # Panics
///
/// Panics if the client is never told the command started, or is told in the
/// wrong order relative to the result.
#[test_log::test(tokio::test)]
async fn a_running_command_is_reported_in_progress_before_it_finishes() -> Result<(), Box<dyn Error>>
{
    let mut serve = Serve::start().await?;
    run_prompt(&mut serve, "!tool run_command echo hello").await?;

    let statuses: Vec<&str> = serve
        .updates("tool_call_update")
        .into_iter()
        .filter_map(|update| update.get("status").and_then(Value::as_str))
        .collect();

    assert_eq!(
        statuses,
        vec!["in_progress", "completed"],
        "the client could not tell a running command from a queued one"
    );

    // Order is the point, not just presence. Reporting progress from the turn
    // loop would put in_progress before the permission request, claiming the
    // command was running while the adapter sat waiting for the user to approve
    // it — which is the failure this whole change exists to avoid.
    let asked = serve
        .position_of_method("session/request_permission")
        .ok_or("the client was never asked for permission")?;
    let running = serve
        .position_of_status("in_progress")
        .ok_or("the client was never told the command started")?;
    assert!(
        asked < running,
        "in_progress arrived before the permission prompt, so it reported work \
         that had not started"
    );
    Ok(())
}

/// A command that fails must be reported as failed, not quietly completed.
///
/// # Errors
///
/// Returns an error if the turn does not complete in time.
///
/// # Panics
///
/// Panics if a failing command is reported as completed.
#[test_log::test(tokio::test)]
async fn a_failing_command_is_reported_as_failed() -> Result<(), Box<dyn Error>> {
    let mut serve = Serve::start().await?;
    run_prompt(&mut serve, "!tool run_command exit 3").await?;

    let updates = serve.updates("tool_call_update");
    let update = updates
        .last()
        .ok_or("no tool_call_update reached the client")?;
    assert_eq!(
        update.get("status").and_then(Value::as_str),
        Some("failed"),
        "a command exiting non-zero was not reported as failed: {update}"
    );
    Ok(())
}

/// Pressing stop must end the turn, kill the command, and keep the session.
///
/// This is the path an editor's stop button takes, and the one nothing else
/// covers: the unit tests cancel a token they own, and the disconnect test drops
/// the whole serve future, which would still pass if `session/cancel` did
/// nothing at all (daa-88aj).
///
/// Linux-only because it reads `/proc` for the descendant's state; the behaviour
/// it guards applies to every unix.
///
/// # Errors
///
/// Returns an error if the command never starts or a turn does not complete.
///
/// # Panics
///
/// Panics if the turn is not reported cancelled, the command survives, or the
/// session cannot be used afterwards.
#[cfg(target_os = "linux")]
#[test_log::test(tokio::test)]
async fn session_cancel_stops_the_command_and_leaves_the_session_usable()
-> Result<(), Box<dyn Error>> {
    let pid_file = std::env::temp_dir().join(format!(
        "acp-serve-cancel-descendant-{}.pid",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_file);

    let mut serve = Serve::start().await?;
    let command = backgrounding_command(&pid_file);
    let prompt = serve
        .start_prompt(&format!("!tool run_command {command}"))
        .await?;

    let started = {
        let pid_file = pid_file.clone();
        serve
            .pump(Duration::from_secs(20), None, move || pid_file.exists())
            .await?
    };
    assert!(
        matches!(started, Stopped::Predicate),
        "the command never started: {started:?}"
    );
    let descendant = std::fs::read_to_string(&pid_file)?.trim().to_owned();
    let _ = std::fs::remove_file(&pid_file);
    assert!(
        alive(&descendant),
        "the descendant was gone before we began"
    );

    // The stop button: a notification, with the connection left open.
    let session_id = serve.session_id().to_owned();
    serve
        .notify("session/cancel", &json!({"sessionId": session_id}))
        .await?;

    let cancelled = serve
        .pump(Duration::from_secs(20), Some(prompt), || false)
        .await?;
    let Stopped::Response(response) = cancelled else {
        return Err(format!("the cancelled turn never returned: {cancelled:?}").into());
    };
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("cancelled"),
        "unexpected outcome for a cancelled turn: {response}"
    );

    // The editor must see the call resolve rather than sit pending forever.
    let updates = serve.updates("tool_call_update");
    let last = updates
        .last()
        .ok_or("no tool_call_update reached the client")?;
    assert_eq!(last.get("status").and_then(Value::as_str), Some("failed"));
    assert!(
        last.to_string().contains("cancelled"),
        "the client was not told why the call ended: {last}"
    );

    let gone_by = tokio::time::Instant::now() + Duration::from_secs(5);
    while alive(&descendant) {
        assert!(
            tokio::time::Instant::now() < gone_by,
            "descendant {descendant} outlived the cancelled turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Cancelling a turn must not take the session with it.
    let after = run_prompt(&mut serve, "still there?").await?;
    assert_eq!(
        after.pointer("/result/stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "the session was unusable after a cancelled turn: {after}"
    );
    Ok(())
}
