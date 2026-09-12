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
use std::time::Duration;

use serde_json::{Value, json};

use acp_client::{Serve, Stopped, alive, backgrounding_command};

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

    // Resolved afterwards, against the same id, carrying the output.
    let updates = serve.updates("tool_call_update");
    let [update] = updates.as_slice() else {
        return Err(format!(
            "expected exactly one tool_call_update, got {}",
            updates.len()
        )
        .into());
    };
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
    let [update] = updates.as_slice() else {
        return Err(format!(
            "expected exactly one tool_call_update, got {}",
            updates.len()
        )
        .into());
    };
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
