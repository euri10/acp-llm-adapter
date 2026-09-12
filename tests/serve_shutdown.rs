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

//! Integration tests for the serve shutdown path.
//!
//! Regression guard for dangling `acp-llm-adapter serve` processes: when the
//! ACP client closes stdin, the adapter must exit instead of hanging forever,
//! and must not leave the work it was doing running behind it.

mod acp_client;

use std::error::Error;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use acp_client::{Serve, Stopped, alive, backgrounding_command};

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

/// A disconnecting client must take a running command's descendants with it.
///
/// The half of daa-zsjw that could not be verified until the mock backend could
/// emit a tool call (daa-wu5e): this drives the real binary over real ACP, so it
/// covers the path an editor drives rather than the function in isolation — the
/// tool call, the permission round-trip, and the shutdown.
///
/// Linux-only because it reads `/proc` for the descendant's state; the
/// behaviour it guards applies to every unix.
///
/// # Errors
///
/// Returns an error if the tool call or the shutdown does not complete in time.
///
/// # Panics
///
/// Panics if the command never starts, serve exits unsuccessfully, or the
/// descendant outlives the disconnected client.
#[cfg(target_os = "linux")]
#[test_log::test(tokio::test)]
async fn serve_shutdown_takes_a_running_commands_descendants_with_it() -> Result<(), Box<dyn Error>>
{
    let pid_file = std::env::temp_dir().join(format!(
        "acp-serve-shutdown-descendant-{}.pid",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_file);

    let mut serve = Serve::start().await?;
    let command = backgrounding_command(&pid_file);
    serve
        .start_prompt(&format!("!tool run_command {command}"))
        .await?;

    // Run the turn until the command proves it started by recording its
    // descendant; permission is granted inside the pump.
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
    assert!(
        alive(&descendant),
        "the descendant was gone before we began"
    );

    // Disconnect mid-command, exactly as closing an editor does.
    serve.disconnect();
    let status = serve.wait(Duration::from_secs(10)).await?;
    assert!(status.success(), "serve exited unsuccessfully: {status:?}");

    let gone_by = tokio::time::Instant::now() + Duration::from_secs(5);
    while alive(&descendant) {
        assert!(
            tokio::time::Instant::now() < gone_by,
            "descendant {descendant} outlived the disconnected client"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}
