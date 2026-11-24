//! Session persistence and reconnect security through the v1 wire protocol.
#![cfg(unix)]

mod acp_client;

use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acp_client::Serve;
use serde_json::{Value, json};
use tokio::process::Command;

const SECRET: &str = "MCP_PERSISTENCE_SECRET_SENTINEL";

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Result<Self, Box<dyn Error>> {
        let path =
            std::env::temp_dir().join(format!("acp-session-security-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn state(&self) -> PathBuf {
        self.0.join("acp-llm-adapter")
    }

    fn session(&self, id: &str) -> PathBuf {
        self.state().join("sessions").join(id)
    }

    fn server(&self, name: &str) -> Result<Value, Box<dyn Error>> {
        let binary = Path::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        let fixture = binary
            .parent()
            .ok_or("missing binary directory")?
            .join("examples/mcp_stdio_fixture");
        Ok(json!({
            "name": name,
            "command": "/bin/sh",
            "args": ["-c", "printf '%s\\n' \"$1\" >> \"$2\"; exec \"$3\"", "fixture", name, self.0.join("connections"), fixture],
            "env": [
                {"name": "ACP_LLM_ADAPTER_RUN_MCP_FIXTURE", "value": "1"},
                {"name": "MCP_FIXTURE_TOKEN", "value": SECRET}
            ]
        }))
    }

    fn connections(&self) -> Result<Vec<String>, Box<dyn Error>> {
        Ok(std::fs::read_to_string(self.0.join("connections"))?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    async fn start(&self, servers: Value, logging: bool) -> Result<Serve, Box<dyn Error>> {
        self.start_with_umask(servers, logging, "000").await
    }

    async fn start_with_umask(
        &self,
        servers: Value,
        logging: bool,
        mask: &str,
    ) -> Result<Serve, Box<dyn Error>> {
        // Control umask in the child, never in the parallel test process.
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "umask \"$1\"; shift; exec \"$@\"", "serve", mask])
            .arg(env!("CARGO_BIN_EXE_acp-llm-adapter"))
            .args(["serve", "--backend", "mock"])
            .env("XDG_STATE_HOME", &self.0)
            .env("ACP_LOG", if logging { "1" } else { "0" })
            .env_remove("ACP_LOG_UNREDACTED");
        Serve::start_with(command, json!({
            "cwd": self.0, "additionalDirectories": [self.0.join("old-root")], "mcpServers": servers
        })).await
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn prompt(serve: &mut Serve, id: &str, text: &str) -> Result<(), Box<dyn Error>> {
    let result = serve
        .request(
            "session/prompt",
            &json!({
                "sessionId": id, "prompt": [{"type": "text", "text": text}]
            }),
        )
        .await?;
    assert!(result.get("result").is_some(), "{result}");
    Ok(())
}

async fn close(serve: &mut Serve, id: &str) -> Result<(), Box<dyn Error>> {
    let result = serve
        .request("session/close", &json!({"sessionId": id}))
        .await?;
    assert!(result.get("result").is_some(), "{result}");
    Ok(())
}

async fn stop(serve: &mut Serve) -> Result<(), Box<dyn Error>> {
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(10)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn v1_session_never_persists_mcp_credentials() -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let mut serve = root.start(json!([root.server("old")?]), false).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "ordinary prompt").await?;
    for file in ["meta.json", "history.jsonl"] {
        let contents = std::fs::read_to_string(root.session(&id).join(file))?;
        assert!(
            !contents.contains(SECRET),
            "MCP credential leaked into {file}"
        );
    }
    stop(&mut serve).await
}

async fn requested_servers_and_roots(method: &str) -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let mut serve = root.start(json!([root.server("old")?]), false).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "persist").await?;
    close(&mut serve, &id).await?;
    let fresh_root = root.0.join("fresh-root");
    let result = serve
        .request(
            method,
            &json!({
                "sessionId": id, "cwd": root.0, "additionalDirectories": [fresh_root],
                "mcpServers": [root.server("fresh")?]
            }),
        )
        .await?;
    assert!(result.get("result").is_some(), "{method}: {result}");
    assert_eq!(root.connections()?, ["old", "fresh"], "{method}");
    let listed = serve.request("session/list", &json!({})).await?;
    let sessions = listed
        .pointer("/result/sessions")
        .and_then(Value::as_array)
        .ok_or("missing sessions")?;
    let restored = sessions
        .iter()
        .find(|session| session.get("sessionId") == Some(&json!(id)))
        .ok_or("missing restored session")?;
    assert_eq!(
        restored.get("additionalDirectories"),
        Some(&json!([fresh_root])),
        "{method}: {restored}"
    );

    close(&mut serve, &id).await?;
    let result = serve
        .request(
            method,
            &json!({
                "sessionId": id, "cwd": root.0, "additionalDirectories": [], "mcpServers": []
            }),
        )
        .await?;
    assert!(result.get("result").is_some(), "{method}: {result}");
    assert_eq!(
        root.connections()?,
        ["old", "fresh"],
        "{method} reconnected an omitted server"
    );
    let listed = serve.request("session/list", &json!({})).await?;
    let sessions = listed
        .pointer("/result/sessions")
        .and_then(Value::as_array)
        .ok_or("missing sessions")?;
    let restored = sessions
        .iter()
        .find(|session| session.get("sessionId") == Some(&json!(id)))
        .ok_or("missing restored session")?;
    assert!(
        restored
            .get("additionalDirectories")
            .is_none_or(|dirs| dirs == &json!([])),
        "{method}: {restored}"
    );
    stop(&mut serve).await?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn v1_load_uses_only_requested_servers_and_roots() -> Result<(), Box<dyn Error>> {
    requested_servers_and_roots("session/load").await
}

#[test_log::test(tokio::test)]
async fn v1_resume_uses_only_requested_servers_and_roots() -> Result<(), Box<dyn Error>> {
    requested_servers_and_roots("session/resume").await
}

#[test_log::test(tokio::test)]
async fn v1_load_scrubs_legacy_mcp_credentials() -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let mut serve = root.start(json!([]), false).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "persist").await?;
    close(&mut serve, &id).await?;
    let path = root.session(&id).join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    meta.as_object_mut()
        .ok_or("metadata is not an object")?
        .insert("mcp_servers".to_string(), json!([root.server("legacy")?]));
    std::fs::write(&path, serde_json::to_vec(&meta)?)?;
    for dir in [
        root.state(),
        root.state().join("sessions"),
        root.session(&id),
    ] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))?;
    }
    for file in [&path, &root.session(&id).join("history.jsonl")] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o666))?;
    }
    let result = serve
        .request(
            "session/load",
            &json!({
                "sessionId": id, "cwd": root.0, "mcpServers": []
            }),
        )
        .await?;
    assert!(result.get("result").is_some(), "{result}");
    let scrubbed = std::fs::read_to_string(path)?;
    assert!(
        !scrubbed.contains(SECRET),
        "legacy credential remains on disk"
    );
    assert!(
        serde_json::from_str::<Value>(&scrubbed)?
            .get("mcp_servers")
            .is_none()
    );
    assert!(
        !root.0.join("connections").exists(),
        "legacy server was reconnected"
    );
    for dir in [
        root.state(),
        root.state().join("sessions"),
        root.session(&id),
    ] {
        assert_eq!(std::fs::metadata(dir)?.permissions().mode() & 0o777, 0o700);
    }
    for file in [
        root.session(&id).join("meta.json"),
        root.session(&id).join("history.jsonl"),
    ] {
        assert_eq!(std::fs::metadata(file)?.permissions().mode() & 0o777, 0o600);
    }
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn corrupt_history_does_not_prevent_legacy_credential_scrubbing() -> Result<(), Box<dyn Error>>
{
    let root = TestRoot::new()?;
    let mut serve = root.start(json!([]), false).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "persist").await?;
    close(&mut serve, &id).await?;
    let meta_path = root.session(&id).join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
    meta.as_object_mut()
        .ok_or("metadata is not an object")?
        .insert("mcp_servers".to_string(), json!([root.server("legacy")?]));
    std::fs::write(&meta_path, serde_json::to_vec(&meta)?)?;
    std::fs::write(root.session(&id).join("history.jsonl"), "invalid history\n")?;
    let response = serve
        .request(
            "session/load",
            &json!({
                "sessionId": id, "cwd": root.0, "mcpServers": []
            }),
        )
        .await?;
    assert!(response.get("error").is_some(), "{response}");
    let scrubbed = std::fs::read_to_string(meta_path)?;
    assert!(!scrubbed.contains(SECRET));
    assert!(
        serde_json::from_str::<Value>(&scrubbed)?
            .get("mcp_servers")
            .is_none()
    );
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn invalid_restore_does_not_modify_legacy_session() -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let mut serve = root.start(json!([]), false).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "persist").await?;
    close(&mut serve, &id).await?;
    let path = root.session(&id).join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    meta.as_object_mut()
        .ok_or("metadata is not an object")?
        .insert("mcp_servers".to_string(), json!([root.server("legacy")?]));
    let original = serde_json::to_vec(&meta)?;
    std::fs::write(&path, &original)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    let response = serve
        .request(
            "session/load",
            &json!({
                "sessionId": id, "cwd": root.0.join("wrong"), "mcpServers": []
            }),
        )
        .await?;
    assert!(response.get("error").is_some(), "{response}");
    assert!(
        std::fs::read(&path)? == original,
        "invalid restore rewrote metadata"
    );
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o666
    );
    meta.as_object_mut()
        .ok_or("metadata is not an object")?
        .insert("session_id".to_string(), json!("different-session"));
    let original = serde_json::to_vec(&meta)?;
    std::fs::write(&path, &original)?;
    let response = serve
        .request(
            "session/resume",
            &json!({
                "sessionId": id, "cwd": root.0, "mcpServers": []
            }),
        )
        .await?;
    assert!(response.get("error").is_some(), "{response}");
    assert!(
        std::fs::read(&path)? == original,
        "invalid identity rewrote metadata"
    );
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o666
    );
    stop(&mut serve).await
}

#[test_log::test(tokio::test)]
async fn session_state_and_logs_are_private_with_permissive_umask() -> Result<(), Box<dyn Error>> {
    assert_private_state("000").await
}

#[test_log::test(tokio::test)]
async fn session_state_and_logs_are_private_with_restrictive_umask() -> Result<(), Box<dyn Error>> {
    assert_private_state("777").await
}

async fn assert_private_state(mask: &str) -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let mut serve = root.start_with_umask(json!([]), true, mask).await?;
    let id = serve.session_id().to_owned();
    prompt(&mut serve, &id, "persist").await?;
    prompt(&mut serve, &id, "/clear").await?;
    stop(&mut serve).await?;

    let mut incorrect = Vec::new();
    for dir in [
        root.state(),
        root.state().join("sessions"),
        root.session(&id),
        root.state().join("connections"),
    ] {
        let mode = std::fs::metadata(&dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            incorrect.push(format!("{}: {mode:o}", dir.display()));
        }
    }
    for file in [
        root.session(&id).join("meta.json"),
        root.session(&id).join("history.jsonl"),
        root.session(&id).join("log.jsonl"),
        root.session(&id).join("log.jsonl.retention"),
    ] {
        let mode = std::fs::metadata(&file)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            incorrect.push(format!("{}: {mode:o}", file.display()));
        }
    }
    for entry in std::fs::read_dir(root.state().join("connections"))? {
        let path = entry?.path();
        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            incorrect.push(format!("{}: {mode:o}", path.display()));
        }
    }
    assert!(incorrect.is_empty(), "non-private state: {incorrect:?}");
    Ok(())
}
