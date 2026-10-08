//! Exercise file authority through real ACP and a loopback provider, without credentials.

mod acp_client;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acp_client::{Serve, Stopped};
use axum::Router;
use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::routing::{get, post};
use serde_json::{Value, json};

struct Fixture {
    root: PathBuf,
    base_url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!("acp-file-roots-{}", uuid::Uuid::new_v4()));
        for directory in ["project", "outside", "extra"] {
            std::fs::create_dir_all(root.join(directory))?;
        }
        std::fs::write(root.join("outside/secret.txt"), "PRIVATE_SENTINEL")?;
        std::fs::write(root.join("project/inside.txt"), "allowed text")?;
        std::fs::write(root.join("extra/extra.txt"), "additional text")?;
        let router = Router::new()
            .route(
                "/models",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "application/json")],
                        json!({"data":[{"id":"fixture"}]}).to_string(),
                    )
                }),
            )
            .route("/chat/completions", post(tool_response));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "file authority fixture failed");
            }
        });
        Ok(Self {
            root,
            base_url,
            server,
        })
    }

    async fn start(&self, delegated: bool) -> Result<Serve, Box<dyn Error>> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        command
            .args(["serve", "--backend", "groq"])
            .env("LLM_API_KEY", "fixture-key")
            .env("LLM_BASE_URL", &self.base_url)
            .env("LLM_MODEL", "fixture")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("ACP_LOG")
            .env_remove("LLM_PRICING");
        Serve::start_with_capabilities(command,
            json!({"cwd": self.root.join("project"), "additionalDirectories":[self.root.join("extra")], "mcpServers":[]}),
            json!({"fs":{"readTextFile":delegated,"writeTextFile":delegated},"terminal":false})).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn tool_response(body: Bytes) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let last = request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last());
    let call = last
        .filter(|message| message.get("role") == Some(&json!("user")))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .and_then(|content| serde_json::from_str::<Value>(content).ok());
    let chunk = if let Some(call) = call {
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"file-call","function":{
            "name":call.get("name"),"arguments":call.get("arguments").map(Value::to_string)
        }}]},"finish_reason":"tool_calls"}]})
    } else {
        json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
    };
    (
        [(CONTENT_TYPE, "text/event-stream")],
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
    )
}

async fn call(serve: &mut Serve, name: &str, arguments: Value) -> Result<(), Box<dyn Error>> {
    let id = serve
        .start_prompt(&json!({"name":name,"arguments":arguments}).to_string())
        .await?;
    match serve
        .pump(Duration::from_secs(10), Some(id), || false)
        .await?
    {
        Stopped::Response(response)
            if response.pointer("/result/stopReason") == Some(&json!("end_turn")) =>
        {
            Ok(())
        }
        other => Err(format!("file tool did not complete: {other:?}").into()),
    }
}

#[tokio::test]
async fn serve_file_roots_reject_outside_paths_before_editor_io_and_recover()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    for delegated in [false, true] {
        let mut serve = fixture.start(delegated).await?;
        for name in ["read_file", "write_file", "edit_file", "list_dir"] {
            let path = if name == "list_dir" {
                fixture.root.join("outside")
            } else {
                fixture.root.join("outside/secret.txt")
            };
            call(&mut serve, name, json!({"path":path, "content":"overwritten","old_text":"PRIVATE_SENTINEL","new_text":"overwritten"})).await?;
            let updates = serve.updates("tool_call_update");
            let final_update = updates.last().ok_or("missing tool result")?;
            assert_eq!(final_update.get("status"), Some(&json!("failed")));
            assert!(!final_update.to_string().contains("PRIVATE_SENTINEL"));
        }
        assert!(
            serve
                .position_of_method("session/request_permission")
                .is_none()
        );
        assert!(serve.position_of_method("fs/read_text_file").is_none());
        assert!(serve.position_of_method("fs/write_text_file").is_none());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("outside/secret.txt"))?,
            "PRIVATE_SENTINEL"
        );
        // Local listing works even when editor file capabilities are enabled.
        call(&mut serve, "list_dir", json!({"path":"."})).await?;
        assert_eq!(
            serve
                .updates("tool_call_update")
                .last()
                .and_then(|update| update.get("status")),
            Some(&json!("completed"))
        );
        if !delegated {
            call(&mut serve, "read_file", json!({"path":"extra.txt"})).await?;
            assert!(
                serve
                    .updates("tool_call_update")
                    .last()
                    .is_some_and(|update| update.to_string().contains("additional text"))
            );
        }
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn cancelling_editor_read_releases_turn_and_ignores_late_replies()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    for late_error in [false, true] {
        let mut serve = fixture.start(true).await?;
        let id = serve
            .start_prompt(
                &json!({"name":"read_file", "arguments":{"path":"inside.txt"}}).to_string(),
            )
            .await?;
        let pending = serve
            .pump(Duration::from_secs(5), Some(id), || false)
            .await?;
        let Stopped::ClientRequest(request) = pending else {
            return Err(format!("expected delegated read, got {pending:?}").into());
        };
        assert_eq!(request.get("method"), Some(&json!("fs/read_text_file")));
        serve
            .notify("session/cancel", &json!({"sessionId":serve.session_id()}))
            .await?;
        let cancelled = serve
            .pump(Duration::from_secs(2), Some(id), || false)
            .await?;
        assert!(
            matches!(&cancelled, Stopped::Response(response)
            if response.pointer("/result/stopReason") == Some(&json!("cancelled"))),
            "cancellation must finish without the read response: {cancelled:?}"
        );
        assert!(serve.position_of_status("completed").is_none());
        assert!(serve.position_of_status("failed").is_some());
        // Reuse the session before replying to the abandoned read.
        let recovered = serve
            .request(
                "session/prompt",
                &json!({"sessionId":serve.session_id(),
            "prompt":[{"type":"text","text":"recover without editor read"}]}),
            )
            .await?;
        assert_eq!(
            recovered.pointer("/result/stopReason"),
            Some(&json!("end_turn"))
        );
        if late_error {
            serve
                .respond_error(
                    &request,
                    json!({"code":-32603,"message":"late read failure"}),
                )
                .await?;
        } else {
            serve
                .respond(&request, json!({"content":"late read content"}))
                .await?;
        }
        let response = serve
            .request(
                "session/prompt",
                &json!({"sessionId":serve.session_id(),
            "prompt":[{"type":"text","text":"still usable after late reply"}]}),
            )
            .await?;
        assert_eq!(
            response.pointer("/result/stopReason"),
            Some(&json!("end_turn"))
        );
        assert!(serve.position_of_status("completed").is_none());
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn plan_mode_during_editor_preflight_prevents_file_mutation() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    for (name, arguments) in [
        (
            "write_file",
            json!({"path":"inside.txt", "content":"changed"}),
        ),
        (
            "edit_file",
            json!({"path":"inside.txt", "old_text":"allowed", "new_text":"changed"}),
        ),
    ] {
        let mut serve = fixture.start(true).await?;
        let id = serve
            .start_prompt(&json!({"name":name,"arguments":arguments}).to_string())
            .await?;
        let mut approved = false;
        let mut switched = false;
        loop {
            match serve
                .pump_with_permission(Duration::from_secs(5), Some(id), || false, None)
                .await?
            {
                Stopped::Permission(permission) => {
                    approved = true;
                    serve
                        .select_permission(
                            permission.get("id").ok_or("missing permission id")?,
                            "allow_once",
                        )
                        .await?;
                }
                Stopped::ClientRequest(request) => {
                    assert_eq!(
                        request.get("method"),
                        Some(&json!("fs/read_text_file")),
                        "Plan must prevent the write: {request}"
                    );
                    if approved {
                        let changed = serve
                            .request(
                                "session/set_mode",
                                &json!({"sessionId":serve.session_id(),"modeId":"plan"}),
                            )
                            .await?;
                        assert!(changed.get("error").is_none(), "{changed}");
                        switched = true;
                    }
                    serve
                        .respond(&request, json!({"content":"allowed text"}))
                        .await?;
                }
                Stopped::Response(response) => {
                    assert_eq!(
                        response.pointer("/result/stopReason"),
                        Some(&json!("end_turn"))
                    );
                    break;
                }
                other => return Err(format!("file turn did not finish: {other:?}").into()),
            }
        }
        assert!(switched, "never exercised the post-approval read");
        assert!(serve.position_of_status("failed").is_some());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("project/inside.txt"))?,
            "allowed text"
        );
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn serve_file_edits_reject_approval_time_changes() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let path = fixture.root.join("project/inside.txt");
    for delegated in [false, true] {
        let mut buffer = "alpha\nuser original\n".to_owned();
        std::fs::write(&path, &buffer)?;
        let mut serve = fixture.start(delegated).await?;
        let id = serve
            .start_prompt(
                &json!({"name":"edit_file", "arguments":{
                    "path":"inside.txt", "old_text":"alpha", "new_text":"beta"
                }})
                .to_string(),
            )
            .await?;
        let mut writes = Vec::new();
        loop {
            match serve
                .pump_with_permission(Duration::from_secs(10), Some(id), || false, None)
                .await?
            {
                Stopped::Permission(request) => {
                    buffer = "alpha\nUSER CHANGED THIS WHILE APPROVING\n".to_owned();
                    if !delegated {
                        std::fs::write(&path, &buffer)?;
                    }
                    serve
                        .select_permission(
                            request.get("id").ok_or("missing permission id")?,
                            "allow_once",
                        )
                        .await?;
                }
                Stopped::ClientRequest(request) => {
                    assert_eq!(
                        request
                            .pointer("/params/path")
                            .and_then(Value::as_str)
                            .map(Path::new),
                        Some(path.as_path())
                    );
                    match request.get("method").and_then(Value::as_str) {
                        Some("fs/read_text_file") => {
                            serve.respond(&request, json!({"content":buffer})).await?;
                        }
                        Some("fs/write_text_file") => {
                            buffer = request
                                .pointer("/params/content")
                                .and_then(Value::as_str)
                                .ok_or("missing write content")?
                                .to_owned();
                            writes.push(buffer.clone());
                            serve.respond(&request, json!({})).await?;
                        }
                        method => {
                            return Err(format!("unexpected editor request: {method:?}").into());
                        }
                    }
                }
                Stopped::Response(response) => {
                    assert_eq!(
                        response.pointer("/result/stopReason"),
                        Some(&json!("end_turn"))
                    );
                    break;
                }
                other => return Err(format!("edit did not finish: {other:?}").into()),
            }
        }
        let updates = serve.updates("tool_call_update");
        let result = updates.last().ok_or("missing edit result")?;
        assert_eq!(result.get("status"), Some(&json!("failed")));
        assert!(
            result
                .to_string()
                .contains("changed while awaiting approval")
        );
        assert!(writes.is_empty(), "stale editor writes: {writes:?}");
        assert_eq!(buffer, "alpha\nUSER CHANGED THIS WHILE APPROVING\n");
        assert_eq!(
            std::fs::read_to_string(&path)?,
            if delegated {
                "alpha\nuser original\n"
            } else {
                "alpha\nUSER CHANGED THIS WHILE APPROVING\n"
            }
        );
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn serve_file_writes_create_missing_editor_files_and_preserve_read_errors()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    for (name, old_text, error_code) in [
        ("new.txt", None, -32002),
        ("existing.txt", Some("old content"), 0),
        ("unreadable.txt", None, -32603),
    ] {
        let path = fixture.root.join("project").join(name);
        if let Some(content) = old_text {
            std::fs::write(&path, content)?;
        }
        let mut serve = fixture.start(true).await?;
        let id = serve
            .start_prompt(
                &json!({"name":"write_file", "arguments":{
                    "path":name, "content":"new content"
                }})
                .to_string(),
            )
            .await?;
        let mut writes = 0;
        loop {
            match serve
                .pump(Duration::from_secs(10), Some(id), || false)
                .await?
            {
                Stopped::ClientRequest(request) => {
                    assert_eq!(
                        request
                            .pointer("/params/path")
                            .and_then(Value::as_str)
                            .map(Path::new),
                        Some(path.as_path())
                    );
                    match request.get("method").and_then(Value::as_str) {
                        Some("fs/read_text_file") => {
                            if let Some(content) = old_text {
                                serve.respond(&request, json!({"content":content})).await?;
                            } else {
                                serve.respond_error(&request, json!({"code":error_code,
                                    "message": if error_code == -32002 {"Resource not found"} else {"Internal error"}
                                })).await?;
                            }
                        }
                        Some("fs/write_text_file") => {
                            let content = request
                                .pointer("/params/content")
                                .and_then(Value::as_str)
                                .ok_or("missing write content")?;
                            assert_eq!(content, "new content");
                            std::fs::write(&path, content)?;
                            writes += 1;
                            serve.respond(&request, json!({})).await?;
                        }
                        method => {
                            return Err(format!("unexpected editor request: {method:?}").into());
                        }
                    }
                }
                Stopped::Response(response) => {
                    assert_eq!(
                        response.pointer("/result/stopReason"),
                        Some(&json!("end_turn"))
                    );
                    break;
                }
                other => return Err(format!("write did not finish: {other:?}").into()),
            }
        }
        let succeeds = error_code != -32603;
        assert_eq!(writes, usize::from(succeeds));
        let updates = serve.updates("tool_call_update");
        let result = updates.last().ok_or("missing write result")?;
        assert_eq!(
            result.get("status"),
            Some(&json!(if succeeds { "completed" } else { "failed" }))
        );
        if succeeds {
            assert_eq!(std::fs::read_to_string(&path)?, "new content");
            let diff = result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|content| {
                    content
                        .iter()
                        .find(|item| item.get("type") == Some(&json!("diff")))
                })
                .ok_or("missing native diff")?;
            assert_eq!(diff.get("oldText").and_then(Value::as_str), old_text);
            assert_eq!(
                diff.get("newText").and_then(Value::as_str),
                Some("new content")
            );
        } else {
            assert!(!path.exists());
        }
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn serve_search_roots_ignore_external_configuration_and_fail_closed_on_escaping_rules()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let cwd = fixture.root.join("project");
    std::fs::create_dir(cwd.join(".git"))?;
    std::fs::create_dir_all(fixture.root.join("config/git"))?;
    std::fs::write(fixture.root.join(".ignore"), "inside.txt\n")?;
    std::fs::write(fixture.root.join("config/git/ignore"), "inside.txt\n")?;
    std::fs::write(fixture.root.join("outside/rules"), "inside.txt\n")?;
    std::os::unix::fs::symlink(fixture.root.join("outside"), cwd.join("escape"))?;
    std::os::unix::fs::symlink(
        fixture.root.join("outside/secret.txt"),
        cwd.join("leak.txt"),
    )?;
    for delegated in [false, true] {
        let mut serve = fixture.start(delegated).await?;
        for (name, pattern) in [("glob", "**/*.txt"), ("grep", "allowed|PRIVATE|additional")] {
            call(&mut serve, name, json!({"pattern":pattern})).await?;
            let updates = serve.updates("tool_call_update");
            let result = updates.last().ok_or("missing search result")?;
            assert_eq!(result.get("status"), Some(&json!("completed")));
            let content = result.to_string();
            assert!(content.contains("inside.txt"));
            for forbidden in ["secret.txt", "PRIVATE_SENTINEL", "leak.txt", "extra.txt"] {
                assert!(!content.contains(forbidden), "search leaked {forbidden}");
            }
        }
        std::os::unix::fs::symlink(fixture.root.join("outside/rules"), cwd.join(".gitignore"))?;
        for name in ["glob", "grep"] {
            call(&mut serve, name, json!({"pattern":"inside"})).await?;
            assert_eq!(
                serve
                    .updates("tool_call_update")
                    .last()
                    .and_then(|update| update.get("status")),
                Some(&json!("failed"))
            );
        }
        std::fs::remove_file(cwd.join(".gitignore"))?;
        call(&mut serve, "grep", json!({"pattern":"allowed"})).await?;
        assert_eq!(
            serve
                .updates("tool_call_update")
                .last()
                .and_then(|update| update.get("status")),
            Some(&json!("completed"))
        );
        assert!(serve.position_of_method("fs/read_text_file").is_none());
        assert!(
            serve
                .position_of_method("session/request_permission")
                .is_none()
        );
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}
