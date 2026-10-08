//! Terminal RPC cancellation, late creation ownership, and unresponsive cleanup.

mod acp_client;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use acp_client::{Serve, Stopped};
use serde_json::{Value, json};
use tokio::process::Command;

async fn start() -> Result<(Serve, PathBuf), Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!("acp-terminal-{}", uuid::Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
    command
        .args(["serve", "--backend", "mock"])
        .env("XDG_STATE_HOME", &root)
        .env_remove("ACP_LOG");
    let serve = Serve::start_with_capabilities(
        command,
        json!({"cwd": "/tmp", "mcpServers": []}),
        json!({"terminal": true}),
    )
    .await?;
    Ok((serve, root))
}

async fn client_request(serve: &mut Serve, method: &str) -> Result<Value, Box<dyn Error>> {
    let stopped = serve.pump(Duration::from_secs(3), None, || false).await?;
    let Stopped::ClientRequest(request) = stopped else {
        return Err(format!("expected {method}, got {stopped:?}").into());
    };
    assert_eq!(request.get("method").and_then(Value::as_str), Some(method));
    Ok(*request)
}

async fn cancelled(serve: &mut Serve, prompt_id: u64) -> Result<(), Box<dyn Error>> {
    let stopped = serve
        .pump(Duration::from_secs(4), Some(prompt_id), || false)
        .await?;
    let Stopped::Response(response) = stopped else {
        return Err(
            format!("cancel must finish without the pending editor response: {stopped:?}").into(),
        );
    };
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("cancelled")
    );
    Ok(())
}

async fn recover_and_stop(mut serve: Serve, root: PathBuf) -> Result<(), Box<dyn Error>> {
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId": serve.session_id(),
        "prompt": [{"type": "text", "text": "hello again"}]}),
        )
        .await?;
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("end_turn")
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn cancel_pending_create_reaps_late_terminal_and_preserves_session()
-> Result<(), Box<dyn Error>> {
    let (mut serve, root) = start().await?;
    let prompt_id = serve.start_prompt("!tool run_command echo fixture").await?;
    let create = client_request(&mut serve, "terminal/create").await?;
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    cancelled(&mut serve, prompt_id).await?;
    serve
        .respond(&create, json!({"terminalId": "late-terminal"}))
        .await?;
    let kill = client_request(&mut serve, "terminal/kill").await?;
    assert_eq!(
        kill.pointer("/params/terminalId").and_then(Value::as_str),
        Some("late-terminal")
    );
    // A hung kill must not prevent release of the same terminal.
    let release = client_request(&mut serve, "terminal/release").await?;
    assert_eq!(
        release.pointer("/params/terminalId"),
        kill.pointer("/params/terminalId")
    );
    serve.respond(&release, json!({})).await?;
    recover_and_stop(serve, root).await
}

#[test_log::test(tokio::test)]
async fn disconnect_reaps_pending_terminal_create_owner() -> Result<(), Box<dyn Error>> {
    let (mut serve, root) = start().await?;
    let prompt_id = serve.start_prompt("!tool run_command echo fixture").await?;
    client_request(&mut serve, "terminal/create").await?;
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    cancelled(&mut serve, prompt_id).await?;
    // No creation response ever arrives; the connection owns and drops that wait.
    recover_and_stop(serve, root).await
}

#[test_log::test(tokio::test)]
async fn cancellation_bounds_wait_output_and_unresponsive_cleanup() -> Result<(), Box<dyn Error>> {
    for pending_output in [false, true] {
        let (mut serve, root) = start().await?;
        let prompt_id = serve.start_prompt("!tool run_command echo fixture").await?;
        let create = client_request(&mut serve, "terminal/create").await?;
        serve
            .respond(&create, json!({"terminalId": "running-terminal"}))
            .await?;
        let wait = client_request(&mut serve, "terminal/wait_for_exit").await?;
        if pending_output {
            serve.respond(&wait, json!({"exitCode": 0})).await?;
            client_request(&mut serve, "terminal/output").await?;
        }
        serve
            .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
            .await?;
        client_request(&mut serve, "terminal/kill").await?;
        client_request(&mut serve, "terminal/release").await?;
        cancelled(&mut serve, prompt_id).await?;
        recover_and_stop(serve, root).await?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn cancellation_during_release_finishes_without_editor_response() -> Result<(), Box<dyn Error>>
{
    let (mut serve, root) = start().await?;
    let prompt_id = serve.start_prompt("!tool run_command echo fixture").await?;
    let create = client_request(&mut serve, "terminal/create").await?;
    serve
        .respond(&create, json!({"terminalId": "finished-terminal"}))
        .await?;
    let wait = client_request(&mut serve, "terminal/wait_for_exit").await?;
    serve.respond(&wait, json!({"exitCode": 0})).await?;
    let output = client_request(&mut serve, "terminal/output").await?;
    serve
        .respond(&output, json!({"output": "fixture", "truncated": false}))
        .await?;
    client_request(&mut serve, "terminal/release").await?;
    serve
        .notify("session/cancel", &json!({"sessionId": serve.session_id()}))
        .await?;
    cancelled(&mut serve, prompt_id).await?;
    recover_and_stop(serve, root).await
}
