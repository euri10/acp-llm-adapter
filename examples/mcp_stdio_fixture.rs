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

//! Stdio MCP server fixture for child-process tests.
//!
//! This is an example target, not a test target: cargo uplifts example
//! artifacts to `<profile>/examples/<name>` under a stable hash-free name, so
//! `mcp_stdio_fixture_path` names the current build exactly instead of scanning
//! `deps/` and picking a stale hash (daa-30py). It also needs rmcp's server
//! features, which live in dev-dependencies and are therefore unavailable to a
//! binary target. `cargo test` builds it without running it.

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock as McpContent, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool as McpTool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt};
use serde_json::Value;

const RUN_FIXTURE_ENV: &str = "ACP_LLM_ADAPTER_RUN_MCP_FIXTURE";

#[derive(Debug, Clone)]
struct StdioFixtureServer {
    launch_arg: String,
    env_token: String,
    list_tools_error: bool,
    invocation_log: Option<PathBuf>,
}

impl StdioFixtureServer {
    async fn record(&self, event: &'static str) -> Result<(), rmcp::ErrorData> {
        let Some(path) = self.invocation_log.clone() else {
            return Ok(());
        };
        blocking::unblock(move || {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{event}")
        })
        .await
        .map_err(|_| rmcp::ErrorData::internal_error("fixture event write failed", None))
    }
}

impl ServerHandler for StdioFixtureServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let message = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        self.record("invoked").await?;
        if message == "hold" {
            tokio::select! {
                () = context.ct.cancelled() => {
                    self.record("cancelled").await?;
                    return Err(rmcp::ErrorData::internal_error("fixture call cancelled", None));
                }
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        Ok(CallToolResult::success(vec![McpContent::text(format!(
            "stdio echo: {message}; arg: {}; env: {}",
            self.launch_arg, self.env_token
        ))]))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        if self.list_tools_error {
            return Err(rmcp::ErrorData::internal_error(
                "simulated list tools failure",
                None,
            ));
        }

        Ok(ListToolsResult {
            tools: vec![McpTool::new(
                "stdio_echo",
                "Echoes a message and launch metadata",
                rmcp::model::object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    },
                    "required": ["message"]
                })),
            )],
            ..Default::default()
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    if std::env::var_os(RUN_FIXTURE_ENV).is_none() {
        return Ok(());
    }

    let server = StdioFixtureServer {
        launch_arg: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "missing-arg".to_string()),
        env_token: std::env::var("MCP_FIXTURE_TOKEN").unwrap_or_else(|_| "missing-env".to_string()),
        list_tools_error: std::env::var("MCP_FIXTURE_MODE").is_ok_and(|mode| mode == "list_error"),
        invocation_log: std::env::var_os("MCP_FIXTURE_INVOCATION_LOG").map(PathBuf::from),
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
