//! v2 tool-service translation and permission lifecycle.

use super::{AcpResult, diff, running, send, v2_tool_kind};
use crate::acp::{
    PermissionRequester, ReadTextFileRequester, TerminalRequester, ToolProgressReporter,
    WriteTextFileRequester,
};
use agent_client_protocol::schema::{v1, v2};
use agent_client_protocol::{Client, V2ConnectionTo};
use futures_util::future::BoxFuture;

/// The tool layer's view of a v2 connection. Permission prompts reach the
/// client as v2 requests; v2 has no client file system or terminal, so those
/// requests are refused (v2 sessions advertise no such capability, so the
/// tool layer runs file and command tools locally and never calls them).
#[derive(Debug)]
pub(super) struct V2Tools {
    pub(super) connection: V2ConnectionTo<Client>,
}

type RequestFuture<'a, T> = BoxFuture<'a, AcpResult<T>>;

fn no_client_capability<'a, T: Send + 'a>(what: &'static str) -> RequestFuture<'a, T> {
    Box::pin(async move {
        Err(agent_client_protocol::Error::invalid_request()
            .data(format!("ACP v2 has no client {what}")))
    })
}

impl ReadTextFileRequester for V2Tools {
    fn read_text_file(
        &self,
        _: v1::ReadTextFileRequest,
    ) -> RequestFuture<'_, v1::ReadTextFileResponse> {
        no_client_capability("file system")
    }
}

impl WriteTextFileRequester for V2Tools {
    fn write_text_file(
        &self,
        _: v1::WriteTextFileRequest,
    ) -> RequestFuture<'_, v1::WriteTextFileResponse> {
        no_client_capability("file system")
    }
}

impl TerminalRequester for V2Tools {
    fn create_terminal(
        &self,
        _: v1::CreateTerminalRequest,
    ) -> RequestFuture<'_, v1::CreateTerminalResponse> {
        no_client_capability("terminal")
    }
    fn terminal_output(
        &self,
        _: v1::TerminalOutputRequest,
    ) -> RequestFuture<'_, v1::TerminalOutputResponse> {
        no_client_capability("terminal")
    }
    fn wait_for_terminal_exit(
        &self,
        _: v1::WaitForTerminalExitRequest,
    ) -> RequestFuture<'_, v1::WaitForTerminalExitResponse> {
        no_client_capability("terminal")
    }
    fn release_terminal(
        &self,
        _: v1::ReleaseTerminalRequest,
    ) -> RequestFuture<'_, v1::ReleaseTerminalResponse> {
        no_client_capability("terminal")
    }
    fn kill_terminal(
        &self,
        _: v1::KillTerminalRequest,
    ) -> RequestFuture<'_, v1::KillTerminalResponse> {
        no_client_capability("terminal")
    }
}

impl PermissionRequester for V2Tools {
    fn request_permission(
        &self,
        request: v1::RequestPermissionRequest,
        tool_name: &str,
        cwd: &std::path::Path,
    ) -> RequestFuture<'_, v1::RequestPermissionResponse> {
        let request = permission_request(request, tool_name, cwd);
        Box::pin(async move {
            let session_id = request.session_id.clone();
            // The turn loop executes tools sequentially, so this permission
            // wait blocks all foreground work. Cancellation drops this future
            // and the turn reports idle without a spurious running update.
            send(
                &self.connection,
                &session_id,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::RequiresAction(
                    v2::RequiresActionStateUpdate::new(),
                )),
            )?;
            let response = self.connection.send_request(request).block_task().await;
            send(&self.connection, &session_id, running())?;
            let response = response?;
            Ok(v1::RequestPermissionResponse::new(match response.outcome {
                v2::RequestPermissionOutcome::Selected(selected) => {
                    v1::RequestPermissionOutcome::Selected(v1::SelectedPermissionOutcome::new(
                        selected.option_id.to_string(),
                    ))
                }
                // Cancelled, or an unknown outcome: deny.
                _ => v1::RequestPermissionOutcome::Cancelled,
            }))
        })
    }
}

impl ToolProgressReporter for V2Tools {
    fn report_in_progress(&self, session_id: &v1::SessionId, tool_call_id: &str) {
        // Best effort, as in v1: a lost progress hint must not fail the tool.
        let _ = send(
            &self.connection,
            &session_id.to_string().into(),
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(tool_call_id.to_string())
                    .status(v2::ToolCallStatus::InProgress),
            ),
        );
    }
}

/// Preserve shared approval options and previews; only the built-in command
/// tool uses a command subject. MCP arguments never establish host-command identity.
fn permission_request(
    request: v1::RequestPermissionRequest,
    tool_name: &str,
    cwd: &std::path::Path,
) -> v2::RequestPermissionRequest {
    let call = request.tool_call;
    let title = call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Run tool".to_owned());
    let mut tool_call = v2::ToolCallUpdate::new(call.tool_call_id.to_string())
        .name(tool_name.to_owned())
        .title(title.clone())
        .status(v2::ToolCallStatus::Pending);
    if let Some(kind) = call.fields.kind {
        tool_call = tool_call.kind(v2_tool_kind(kind.into()));
    }
    let command = (tool_name == "run_command")
        .then(|| {
            call.fields
                .raw_input
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(serde_json::Value::as_str)
        })
        .flatten()
        .map(str::to_owned);
    if let Some(raw_input) = call.fields.raw_input {
        tool_call = tool_call.raw_input(raw_input);
    }
    if let Some(content) = call.fields.content {
        tool_call = tool_call.content(
            content
                .into_iter()
                .filter_map(|item| match item {
                    v1::ToolCallContent::Diff(edit) => Some(diff::content(
                        &edit.path,
                        edit.old_text.as_deref(),
                        &edit.new_text,
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    let options = request
        .options
        .into_iter()
        .map(|option| {
            let kind = match option.kind {
                v1::PermissionOptionKind::AllowOnce => v2::PermissionOptionKind::AllowOnce,
                v1::PermissionOptionKind::AllowAlways => v2::PermissionOptionKind::AllowAlways,
                v1::PermissionOptionKind::RejectOnce => v2::PermissionOptionKind::RejectOnce,
                _ => v2::PermissionOptionKind::RejectAlways,
            };
            v2::PermissionOption::new(option.option_id.to_string(), option.name, kind)
        })
        .collect();
    let subject = match command {
        Some(command) => v2::RequestPermissionSubject::Command(
            v2::CommandPermissionSubject::new(command, cwd.to_path_buf())
                .tool_call_id(v2::ToolCallId::new(call.tool_call_id.to_string())),
        ),
        None => v2::RequestPermissionSubject::ToolCall(Box::new(
            v2::ToolCallPermissionSubject::new(tool_call),
        )),
    };
    v2::RequestPermissionRequest::new(request.session_id.to_string(), title, options)
        .subject(subject)
}
