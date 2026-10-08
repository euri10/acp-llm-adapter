//! MCP session startup, tool mapping, and invocation helpers.

use std::collections::HashMap;
use std::time::Duration;

use acp_llm_adapter::llm::{ToolCall as ChatToolCall, ToolDefinition};
use agent_client_protocol::schema::v1::{
    HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ContentBlock as McpContent, JsonObject,
    ServerResult, Tool as McpTool,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::Value;
use tokio::process::Command as TokioCommand;
use tokio_util::sync::CancellationToken;

use crate::SessionStore;
use crate::tools::ToolKind;
use crate::tools::{ToolContext, ToolExecution, require_tool_permission};
use acp_llm_adapter::error::AdapterError;

/// Prefix used for model-visible MCP tool names.
pub(crate) const MCP_TOOL_PREFIX: &str = "mcp";
const MCP_TOOL_NAME_PREFIX: &str = "mcp__";

/// Permission kind used for all MCP tools.
///
/// MCP servers are external executors with unknown side effects, so they are
/// treated like command execution for approval decisions.
pub(crate) const MCP_TOOL_KIND: ToolKind = ToolKind::Execute;

#[derive(Debug)]
pub(crate) struct McpSession {
    pub(crate) name: String,
    pub(crate) tools: Vec<McpToolMapping>,
    pub(crate) peer: Peer<RoleClient>,
    pub(crate) _service: RunningService<RoleClient, ()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpToolMapping {
    pub(crate) exposed_name: String,
    pub(crate) original_name: String,
    pub(crate) definition: ToolDefinition,
}

#[derive(Debug, Clone)]
pub(crate) struct McpToolTarget {
    pub(crate) server_name: String,
    pub(crate) original_name: String,
    pub(crate) peer: Peer<RoleClient>,
}

/// Return whether a tool name belongs to an MCP-backed tool.
#[must_use]
pub(crate) fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with(MCP_TOOL_NAME_PREFIX)
}

/// Return the explicit ACP permission kind used for MCP tools.
#[must_use]
pub(crate) const fn mcp_tool_kind() -> ToolKind {
    MCP_TOOL_KIND
}

/// Execute an MCP tool call for the given session.
///
/// External tools use Execute permission: Ask and `AcceptEdits` prompt, YOLO and
/// remembered decisions follow the shared policy. Plan and selected-content
/// sessions cannot invoke them, even through direct dispatch. Cancellation
/// drops local work and requests remote cancellation; it cannot undo side effects.
#[must_use]
pub(crate) async fn mcp_tool_execution(
    store: &SessionStore,
    call: &ChatToolCall,
    context: &ToolContext,
    requester: Option<&dyn crate::PermissionRequester>,
    cancellation: &CancellationToken,
) -> ToolExecution {
    let target = match store.find_mcp_target(&context.session_id, call.name()) {
        Ok(Some(target)) => target,
        Ok(None) => return ToolExecution::failed(format!("unknown MCP tool: {}", call.name())),
        Err(error) => return ToolExecution::failed(error.to_string()),
    };

    let arguments = match mcp_call_arguments(call) {
        Ok(arguments) => arguments,
        Err(error) => return ToolExecution::failed(error),
    };

    match store.selected_content_limits(&context.session_id) {
        Ok(Some(_)) => {
            return ToolExecution::failed("selected-content Sessions refuse all tool calls");
        }
        Err(error) => return ToolExecution::failed(error.to_string()),
        Ok(None) => {}
    }
    match store.session_behavior(&context.session_id) {
        Ok(mode) if !mode.allows_tool_kind(MCP_TOOL_KIND) => {
            return ToolExecution::failed("plan mode refuses MCP tool calls");
        }
        Err(error) => return ToolExecution::failed(error.to_string()),
        Ok(_) => {}
    }
    let approval = tokio::select! {
        biased;
        () = cancellation.cancelled() => return ToolExecution::failed("MCP tool call cancelled"),
        result = require_tool_permission(store, context, call, MCP_TOOL_KIND, requester, cancellation) => result,
    };
    if let Err(error) = approval {
        return ToolExecution::failed(error);
    }
    // Cancellation may have arrived as the editor's approval completed.
    if cancellation.is_cancelled() {
        return ToolExecution::failed("MCP tool call cancelled");
    }

    let result = invoke_mcp_tool(&target, arguments, cancellation).await;

    match result {
        Ok(result) => {
            let model_output = mcp_tool_result_text(&result.content);
            let raw_output = serde_json::to_value(&result).unwrap_or_else(|error| {
                serde_json::json!({
                    "error": format!("failed to serialize MCP tool result: {error}")
                })
            });
            ToolExecution {
                content: model_output,
                raw_output,
                success: !result.is_error.unwrap_or(false),
                edit: None,
            }
        }
        Err(error) => ToolExecution::failed(format!(
            "MCP tool '{}' on server '{}' failed: {error}",
            target.original_name, target.server_name
        )),
    }
}

async fn invoke_mcp_tool(
    target: &McpToolTarget,
    arguments: JsonObject,
    cancellation: &CancellationToken,
) -> Result<CallToolResult, String> {
    let mut handle = target
        .peer
        .send_cancellable_request(
            CallToolRequest::new(
                CallToolRequestParams::new(target.original_name.clone()).with_arguments(arguments),
            )
            .into(),
            PeerRequestOptions::no_options(),
        )
        .await
        .map_err(|error| error.to_string())?;
    // No request options install progress watchers. Reading this owned handle's
    // receiver lets cancellation retain the request id for the MCP notification.
    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            match tokio::time::timeout(Duration::from_secs(1), handle.cancel(Some("ACP turn cancelled".to_string()))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::warn!("MCP cancellation notification failed"),
                Err(_) => tracing::warn!("MCP cancellation notification timed out"),
            }
            return Err("MCP tool call cancelled".to_string());
        }
        response = &mut handle.rx => response.map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?,
    };
    match response {
        ServerResult::CallToolResult(result) => Ok(result),
        _ => Err("unexpected MCP tool response".to_string()),
    }
}

/// Parse MCP tool arguments from the model-emitted JSON payload.
pub(crate) fn mcp_call_arguments(call: &ChatToolCall) -> Result<JsonObject, String> {
    match serde_json::from_str::<Value>(call.arguments()) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        Ok(_) => Err(format!(
            "MCP tool '{}' arguments must be a JSON object",
            call.name()
        )),
        Err(error) => Err(format!(
            "invalid MCP tool '{}' arguments: {error}",
            call.name()
        )),
    }
}

/// Render MCP result content into the plain text fed back to the model.
#[must_use]
pub(crate) fn mcp_tool_result_text(content: &[McpContent]) -> String {
    let parts = content
        .iter()
        .map(|content| {
            content.as_text().map_or_else(
                || {
                    serde_json::to_string(content)
                        .unwrap_or_else(|error| format!("failed to serialize MCP content: {error}"))
                },
                |text| text.text.clone(),
            )
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    }
}

/// Connect all requested MCP servers for a new ACP session.
///
/// # Errors
///
/// Returns an ACP error when a server transport is unsupported, transport
/// configuration is invalid, initialization fails, or the server cannot be
/// queried for tools.
pub(crate) async fn connect_mcp_sessions(
    servers: &[McpServer],
) -> Result<Vec<McpSession>, AdapterError> {
    let mut sessions = Vec::new();

    for server in servers {
        match server {
            McpServer::Stdio(stdio) => sessions.push(connect_mcp_stdio_session(stdio).await?),
            McpServer::Http(http) => sessions.push(connect_mcp_http_session(http).await?),
            McpServer::Sse(sse) => sessions.push(connect_mcp_sse_session(sse).await?),
            _ => {
                return Err(AdapterError::InvalidParams(
                    "unsupported MCP server transport".to_string(),
                ));
            }
        }
    }

    Ok(sessions)
}

/// Connect a single stdio MCP server and collect its advertised tools.
///
/// # Errors
///
/// Returns an ACP error when the command path is not absolute, the process
/// fails to start, initialization fails, or tool discovery fails.
pub(crate) async fn connect_mcp_stdio_session(
    server: &McpServerStdio,
) -> Result<McpSession, AdapterError> {
    if !server.command.is_absolute() {
        return Err(AdapterError::InvalidParams(format!(
            "MCP server '{}' command must be absolute",
            server.name
        )));
    }

    let command = TokioCommand::new(&server.command).configure(|command| {
        command.args(&server.args);
        for variable in &server.env {
            command.env(&variable.name, &variable.value);
        }
    });
    let transport = TokioChildProcess::new(command).map_err(|error| {
        AdapterError::InvalidParams(format!(
            "failed to start MCP server '{}': {error}",
            server.name
        ))
    })?;
    let service = ().serve(transport).await.map_err(|error| {
        AdapterError::InvalidParams(format!(
            "failed to initialize MCP server '{}': {error}",
            server.name
        ))
    })?;
    mcp_session_from_service(&server.name, service).await
}

/// Connect a single streamable HTTP MCP server and collect its advertised tools.
///
/// # Errors
///
/// Returns an ACP error when headers are invalid, initialization fails, or tool
/// discovery fails.
pub(crate) async fn connect_mcp_http_session(
    server: &McpServerHttp,
) -> Result<McpSession, AdapterError> {
    let custom_headers = mcp_http_headers(&server.headers, &server.name)?;
    let config = StreamableHttpClientTransportConfig::with_uri(server.url.clone())
        .custom_headers(custom_headers);
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = ().serve(transport).await.map_err(|error| {
        AdapterError::InvalidParams(format!(
            "failed to initialize MCP server '{}': {error}",
            server.name
        ))
    })?;

    mcp_session_from_service(&server.name, service).await
}

/// Connect a single SSE MCP server and collect its advertised tools.
///
/// This uses the rmcp streamable HTTP client transport for session startup and
/// tool RPC, which is compatible with ACP-declared SSE MCP server entries.
///
/// # Errors
///
/// Returns an ACP error when headers are invalid, initialization fails, or tool
/// discovery fails.
pub(crate) async fn connect_mcp_sse_session(
    server: &McpServerSse,
) -> Result<McpSession, AdapterError> {
    let custom_headers = mcp_http_headers(&server.headers, &server.name)?;
    let config = StreamableHttpClientTransportConfig::with_uri(server.url.clone())
        .custom_headers(custom_headers);
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = ().serve(transport).await.map_err(|error| {
        AdapterError::InvalidParams(format!(
            "failed to initialize MCP server '{}': {error}",
            server.name
        ))
    })?;

    mcp_session_from_service(&server.name, service).await
}

async fn mcp_session_from_service(
    server_name: &str,
    service: RunningService<RoleClient, ()>,
) -> Result<McpSession, AdapterError> {
    let peer = service.peer().clone();
    let tools = peer.list_all_tools().await.map_err(|error| {
        AdapterError::InvalidParams(format!(
            "failed to list MCP tools for server '{server_name}': {error}",
        ))
    })?;
    let mappings = mcp_tool_mappings(server_name, tools);

    Ok(McpSession {
        name: server_name.to_string(),
        tools: mappings,
        peer,
        _service: service,
    })
}

fn mcp_http_headers(
    headers: &[HttpHeader],
    server_name: &str,
) -> Result<HashMap<HeaderName, HeaderValue>, AdapterError> {
    let mut parsed = HashMap::with_capacity(headers.len());
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
            AdapterError::InvalidParams(format!(
                "invalid HTTP header name '{}' for MCP server '{server_name}': {error}",
                header.name
            ))
        })?;
        let value = HeaderValue::from_str(&header.value).map_err(|error| {
            AdapterError::InvalidParams(format!(
                "invalid HTTP header value for '{}' on MCP server '{server_name}': {error}",
                header.name
            ))
        })?;
        parsed.insert(name, value);
    }
    Ok(parsed)
}

/// Map MCP server tool metadata into model-visible tool definitions.
#[must_use]
pub(crate) fn mcp_tool_mappings(server_name: &str, tools: Vec<McpTool>) -> Vec<McpToolMapping> {
    tools
        .into_iter()
        .map(|tool| {
            let original_name = tool.name.to_string();
            let exposed_name = mcp_tool_name(server_name, &original_name);
            let description = tool.description.map_or_else(
                || format!("MCP tool '{original_name}' from server '{server_name}'"),
                |description| description.to_string(),
            );
            // The OpenAI-compatible API requires tool parameters to have type: "object" at the root.
            // MCP schemas might not have this structure, so we ensure it's properly wrapped.
            let schema = validate_tool_schema(tool.input_schema.as_ref());
            let definition = ToolDefinition::new(exposed_name.clone(), description, schema);

            McpToolMapping {
                exposed_name,
                original_name,
                definition,
            }
        })
        .collect()
}

/// Validate and normalize a tool schema for provider API compatibility.
///
/// The provider requires tool parameters to be a JSON schema with `type: "object"` at the root.
/// If the schema doesn't have this structure, wrap it appropriately.
fn validate_tool_schema(schema: &JsonObject) -> Value {
    let schema_value = Value::Object(schema.clone());

    // Check if the schema already has type: "object"
    if let Some(Value::String(type_val)) = schema_value.get("type")
        && type_val == "object"
    {
        return schema_value;
    }

    // If not, wrap it in an object schema where the original schema becomes properties
    serde_json::json!({
        "type": "object",
        "properties": {
            "value": schema_value
        },
        "required": ["value"]
    })
}

fn mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "{MCP_TOOL_PREFIX}__{}__{}",
        sanitize_tool_name_part(server_name),
        sanitize_tool_name_part(tool_name)
    )
}

pub(crate) fn sanitize_tool_name_part(value: &str) -> String {
    let mut sanitized = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            sanitized.push(character.to_ascii_lowercase());
        } else {
            sanitized.push('_');
        }
    }

    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests;
