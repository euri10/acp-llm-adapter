//! Exercise file authority through real ACP and a loopback provider, without credentials.

mod acp_client;

use std::error::Error;
use std::path::PathBuf;
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
