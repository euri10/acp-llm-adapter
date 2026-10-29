//! Promotion coverage using the shipped binary and the existing fake provider.

use super::*;
use std::path::PathBuf;

fn example(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(env!("CARGO_BIN_EXE_acp-llm-adapter"))
        .parent()
        .ok_or("binary has no parent")?
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

#[tokio::test]
async fn inactive_sessions_use_v2_errors_without_admission() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    for (method, params) in [
        (
            "session/prompt",
            json!({"sessionId":"unknown", "prompt":[{"type":"text","text":"hello"}]}),
        ),
        (
            "session/prompt",
            json!({"sessionId":"unknown", "prompt":[{"type":"text","text":"/clear"}]}),
        ),
        (
            "session/set_config_option",
            json!({"sessionId":"unknown","configId":"mode","type":"id","value":"ask"}),
        ),
        ("session/close", json!({"sessionId":"unknown"})),
        (
            "session/resume",
            json!({"sessionId":"unknown","cwd":std::env::temp_dir()}),
        ),
    ] {
        let response = connection.request(method, params).await?;
        assert_eq!(
            response.pointer("/error/code"),
            Some(&json!(-32002)),
            "{method}: {response}"
        );
    }
    assert!(connection.notifications.is_empty());
    let cancel =
        json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":"unknown"}});
    connection
        .stdin
        .write_all(format!("{cancel}\n").as_bytes())
        .await?;
    let response = connection.request("session/list", json!({})).await?;
    assert!(
        response.get("result").is_some(),
        "unknown cancellation closed the connection"
    );
    // Corrupt storage is an internal error, not an unknown-session result.
    let (session, _) = connection.new_session(&std::env::temp_dir()).await?;
    let mark = connection.notifications.len();
    connection
        .request(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"hello"}]}),
        )
        .await?;
    connection.until_idle(mark).await?;
    connection
        .request("session/close", json!({"sessionId":session}))
        .await?;
    std::fs::write(
        connection
            .state_dir
            .join("acp-llm-adapter/sessions")
            .join(&session)
            .join("meta.json"),
        "{",
    )?;
    let resumed = connection
        .request(
            "session/resume",
            json!({"sessionId":session,"cwd":std::env::temp_dir()}),
        )
        .await?;
    assert_eq!(
        resumed.pointer("/error/code"),
        Some(&json!(-32603)),
        "{resumed}"
    );
    connection.stop().await
}

async fn provider() -> Result<(tokio::process::Child, String), Box<dyn Error>> {
    let mut child = Command::new(example("fake_provider")?)
        .env("FAKE_PROVIDER_KEY", "fixture-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let url = tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(child.stdout.take().ok_or("no provider stdout")?)
            .lines()
            .next_line(),
    )
    .await??
    .ok_or("no provider URL")?;
    Ok((child, url))
}

async fn start(url: &str) -> Result<V2Connection, Box<dyn Error>> {
    V2Connection::start_with(
        &["serve", "--backend", "groq"],
        &[
            ("LLM_API_KEY", "fixture-key"),
            ("LLM_BASE_URL", url),
            ("LLM_MODEL", "fixture-model"),
        ],
    )
    .await
}

async fn prompt(
    connection: &mut V2Connection,
    session: &str,
    name: &str,
    args: Value,
) -> Result<usize, Box<dyn Error>> {
    let mark = connection.notifications.len();
    let response = connection
        .request(
            "session/prompt",
            json!({"sessionId": session,
        "prompt": [{"type":"text", "text": format!("!tool {name} {args}")}]}),
        )
        .await?;
    assert!(
        response.pointer("/result/messageId").is_some(),
        "{response}"
    );
    Ok(mark)
}

async fn approve(
    connection: &mut V2Connection,
    permission: &Value,
    allow: bool,
) -> Result<(), Box<dyn Error>> {
    connection
        .respond(
            permission,
            json!({"outcome": {"outcome":"selected",
        "optionId": if allow {"allow_once"} else {"reject_once"}}}),
        )
        .await
}

#[tokio::test]
async fn edits_preview_and_report_v2_changes_before_and_after_approval()
-> Result<(), Box<dyn Error>> {
    let (mut provider, url) = provider().await?;
    let mut connection = start(&url).await?;
    let root = connection.state_dir.join("project");
    std::fs::create_dir_all(&root)?;
    let (session, _) = connection.new_session(&root).await?;
    for (name, args, old, new, operation, allow) in [
        (
            "write_file",
            json!({"path":"note.txt", "content":"one\n"}),
            None,
            "one\n",
            "add",
            true,
        ),
        (
            "edit_file",
            json!({"path":"note.txt", "old_text":"one", "new_text":"two"}),
            Some("one\n"),
            "two\n",
            "modify",
            true,
        ),
        (
            "write_file",
            json!({"path":"note.txt", "content":"denied\n"}),
            Some("two\n"),
            "denied\n",
            "modify",
            false,
        ),
    ] {
        let mark = prompt(&mut connection, &session, name, args).await?;
        let permission = connection
            .wait_request("session/request_permission")
            .await?;
        assert_eq!(permission["params"]["subject"]["type"], "tool_call");
        let preview = &permission["params"]["subject"]["toolCall"]["content"][0];
        assert_eq!(preview["type"], "diff", "{permission}");
        assert_eq!(preview["changes"][0]["operation"], operation);
        assert_eq!(preview["changes"][0]["path"], json!(root.join("note.txt")));
        assert_eq!(preview["patch"]["format"], "git_patch");
        assert!(
            preview["patch"]["text"]
                .as_str()
                .ok_or("no patch")?
                .contains(&format!("+{new}"))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt"))
                .ok()
                .as_deref(),
            old
        );
        approve(&mut connection, &permission, allow).await?;
        let updates = connection.until_idle(mark).await?;
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt"))?,
            if allow {
                new
            } else {
                old.ok_or("no old text")?
            }
        );
        if allow {
            assert!(
                tool_updates(&updates)
                    .iter()
                    .any(|u| u["status"] == "completed" && &u["content"][0] == preview),
                "{updates:?}"
            );
        }
    }
    connection.stop().await?;
    drop(provider.stdin.take());
    provider.wait().await?;
    Ok(())
}

#[tokio::test]
async fn additional_roots_are_confined_and_can_be_revoked_on_resume() -> Result<(), Box<dyn Error>>
{
    let mut connection = V2Connection::start().await?;
    let root = connection.state_dir.join("project");
    let extra = connection.state_dir.join("extra");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&extra)?;
    std::fs::write(extra.join("note"), "approved-root")?;
    let created = connection
        .request(
            "session/new",
            json!({"cwd": root, "additionalDirectories": [extra]}),
        )
        .await?;
    let session = created["result"]["sessionId"]
        .as_str()
        .ok_or_else(|| format!("{created}"))?
        .to_owned();
    connection.until_idle(0).await?;
    for allowed in [true, false] {
        let mark = connection.notifications.len();
        connection
            .request(
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text",
            "text":format!("!tool read_file {}", extra.join("note").display())}]}),
            )
            .await?;
        let updates = connection.until_idle(mark).await?;
        assert!(
            tool_updates(&updates)
                .iter()
                .any(|u| u["status"] == if allowed { "completed" } else { "failed" }),
            "{updates:?}"
        );
        if allowed {
            connection
                .request("session/close", json!({"sessionId":session}))
                .await?;
            let resumed = connection
                .request(
                    "session/resume",
                    json!({"sessionId":session,"cwd":root,"additionalDirectories":[]}),
                )
                .await?;
            assert!(resumed.get("result").is_some(), "{resumed}");
        }
    }
    connection.stop().await
}

#[tokio::test]
async fn stdio_mcp_is_gated_and_resume_uses_fresh_servers() -> Result<(), Box<dyn Error>> {
    let (mut provider, url) = provider().await?;
    let mut connection = start(&url).await?;
    let root = connection.state_dir.join("project");
    std::fs::create_dir_all(&root)?;
    let servers = json!([{"type":"stdio", "name":"audit", "command":example("mcp_stdio_fixture")?,
        "args":["v2-arg"], "env":[{"name":"ACP_LLM_ADAPTER_RUN_MCP_FIXTURE","value":"1"},
        {"name":"MCP_FIXTURE_TOKEN", "value":"V2_MCP_PRIVATE_SENTINEL"}]}]);
    let created = connection
        .request("session/new", json!({"cwd":root,"mcpServers":servers}))
        .await?;
    let session = created["result"]["sessionId"]
        .as_str()
        .ok_or_else(|| format!("{created}"))?
        .to_owned();
    connection.until_idle(0).await?;
    let mark = prompt(
        &mut connection,
        &session,
        "mcp__audit__stdio_echo",
        json!({"message":"hello", "command":"not a host command"}),
    )
    .await?;
    let permission = connection
        .wait_request("session/request_permission")
        .await?;
    assert_eq!(permission["params"]["subject"]["type"], "tool_call");
    approve(&mut connection, &permission, true).await?;
    let updates = connection.until_idle(mark).await?;
    assert!(
        tool_updates(&updates)
            .iter()
            .any(|u| u["status"] == "completed" && u.to_string().contains("stdio echo: hello")),
        "{updates:?}"
    );
    // Credentials belong to this connection, not persisted MCP configuration.
    let meta = std::fs::read_to_string(
        connection
            .state_dir
            .join("acp-llm-adapter/sessions")
            .join(&session)
            .join("meta.json"),
    )?;
    assert!(!meta.contains("V2_MCP_PRIVATE_SENTINEL"));
    connection
        .request("session/close", json!({"sessionId":session}))
        .await?;
    let resumed = connection
        .request(
            "session/resume",
            json!({"sessionId":session,"cwd":root,"mcpServers":[]}),
        )
        .await?;
    assert!(resumed.get("result").is_some(), "{resumed}");
    let mark = prompt(
        &mut connection,
        &session,
        "mcp__audit__stdio_echo",
        json!({"message":"removed"}),
    )
    .await?;
    let updates = connection.until_idle(mark).await?;
    assert!(
        tool_updates(&updates)
            .iter()
            .any(|u| u["status"] == "failed"),
        "{updates:?}"
    );
    assert!(
        connection.incoming.is_empty(),
        "removed MCP server asked for approval"
    );
    connection.stop().await?;
    drop(provider.stdin.take());
    provider.wait().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_during_approval_ignores_late_answers() -> Result<(), Box<dyn Error>> {
    let mut connection = V2Connection::start().await?;
    let root = connection.state_dir.join("project");
    std::fs::create_dir_all(&root)?;
    let (session, _) = connection.new_session(&root).await?;
    let mark = connection.notifications.len();
    connection
        .request(
            "session/prompt",
            json!({"sessionId":session,"prompt":[
        {"type":"text","text":"!tool run_command touch must-not-exist"}]}),
        )
        .await?;
    let permission = connection
        .wait_request("session/request_permission")
        .await?;
    let cancel =
        json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":session}});
    connection
        .stdin
        .write_all(format!("{cancel}\n").as_bytes())
        .await?;
    let updates = connection.until_idle(mark).await?;
    let states: Vec<_> = updates
        .iter()
        .filter_map(|u| u.get("state"))
        .cloned()
        .collect();
    assert_eq!(
        states,
        vec![json!("running"), json!("requires_action"), json!("idle")]
    );
    assert_eq!(
        updates.last().and_then(|u| u.get("stopReason")),
        Some(&json!("cancelled"))
    );
    approve(&mut connection, &permission, true).await?;
    let mark = connection.notifications.len();
    connection
        .request(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"hello"}]}),
        )
        .await?;
    connection.until_idle(mark).await?;
    assert!(!root.join("must-not-exist").exists());
    connection.stop().await
}

#[tokio::test]
async fn http_mcp_receives_headers_and_runs_only_after_approval() -> Result<(), Box<dyn Error>> {
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    let (seen, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route("/mcp", post(move |headers: HeaderMap, body: String| {
        let seen = seen.clone();
        async move {
            if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer fixture-token") {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let Ok(request) = serde_json::from_str::<Value>(&body) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Some(id) = request.get("id") else { return StatusCode::ACCEPTED.into_response(); };
            let result = match request["method"].as_str() {
                Some("initialize") => json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{}},
                    "serverInfo":{"name":"http-fixture","version":"1"}}),
                Some("tools/list") => json!({"tools":[{"name":"echo","description":"fixture", "inputSchema":{"type":"object"}}]}),
                Some("tools/call") => {
                    // A dropped test receiver needs no invocation evidence.
                    let _ = seen.send(request.clone());
                    json!({"content":[{"type":"text","text":"http MCP ran"}]})
                }
                _ => return StatusCode::BAD_REQUEST.into_response(),
            };
            ([(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mcp_url = format!("http://{}/mcp", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, app).await });
    let (mut provider, url) = provider().await?;
    let mut connection = start(&url).await?;
    let root = connection.state_dir.join("project");
    std::fs::create_dir_all(&root)?;
    let created = connection.request("session/new", json!({"cwd":root,"mcpServers":[
        {"type":"http","name":"remote","url":mcp_url,"headers":[{"name":"Authorization","value":"Bearer fixture-token"}]}]})).await?;
    let session = created["result"]["sessionId"]
        .as_str()
        .ok_or_else(|| format!("{created}"))?
        .to_owned();
    connection.until_idle(0).await?;
    for allow in [false, true] {
        let mark = prompt(&mut connection, &session, "mcp__remote__echo", json!({})).await?;
        let permission = connection
            .wait_request("session/request_permission")
            .await?;
        assert!(requests.try_recv().is_err(), "MCP invoked before approval");
        approve(&mut connection, &permission, allow).await?;
        let updates = connection.until_idle(mark).await?;
        assert!(
            tool_updates(&updates)
                .iter()
                .any(|u| u["status"] == if allow { "completed" } else { "failed" }),
            "{updates:?}"
        );
        if allow {
            assert!(requests.try_recv().is_ok());
        } else {
            assert!(requests.try_recv().is_err());
        }
    }
    connection.stop().await?;
    drop(provider.stdin.take());
    provider.wait().await?;
    server.shutdown().await;
    Ok(())
}
