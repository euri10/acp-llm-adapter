//! ACP requester traits and their production implementations.
//!
//! These traits abstract over ACP client connections so that tool execution and
//! permission logic can be unit-tested without a real transport.

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, TerminalId, TerminalOutputRequest, TerminalOutputResponse, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, Client};
use futures_util::future::BoxFuture;
use std::time::Duration;
use tokio::sync::oneshot;

type AcpRequestFuture<'a, T> = BoxFuture<'a, Result<T, agent_client_protocol::Error>>;

pub(crate) trait ReadTextFileRequester: Send + Sync {
    fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> AcpRequestFuture<'_, ReadTextFileResponse>;
}

pub(crate) trait WriteTextFileRequester: Send + Sync {
    fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> AcpRequestFuture<'_, WriteTextFileResponse>;
}

/// Trait for all terminal operations via ACP client.
pub(crate) trait TerminalRequester: Send + Sync {
    /// Create a terminal and execute a command.
    ///
    /// Dropping this future must retain ownership of any late-created terminal
    /// until it can be killed/released or the connection closes.
    fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> AcpRequestFuture<'_, CreateTerminalResponse>;

    /// Get the current output and status of a terminal.
    fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> AcpRequestFuture<'_, TerminalOutputResponse>;

    /// Wait for a terminal command to exit.
    fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> AcpRequestFuture<'_, WaitForTerminalExitResponse>;

    /// Release a terminal and free its resources.
    fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> AcpRequestFuture<'_, ReleaseTerminalResponse>;

    /// Kill a terminal's running command without releasing the terminal.
    fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> AcpRequestFuture<'_, KillTerminalResponse>;
}

impl TerminalRequester for agent_client_protocol::ConnectionTo<Client> {
    fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> AcpRequestFuture<'_, CreateTerminalResponse> {
        Box::pin(async move {
            let connection = self.clone();
            let session_id = request.session_id.clone();
            let (result_tx, result_rx) = oneshot::channel();
            let (accepted_tx, accepted_rx) = oneshot::channel();
            // The connection owns this wait, even if the turn is cancelled or
            // its session closes before terminal/create returns an identity.
            self.spawn(async move {
                if result_tx.is_closed() {
                    return Ok(());
                }
                match connection.send_request(request).block_task().await {
                    Ok(response) => {
                        let terminal_id = response.terminal_id.clone();
                        // Delivery alone is not acceptance: cancellation can
                        // win while the response is still queued in the channel.
                        if result_tx.send(Ok(response)).is_ok() && accepted_rx.await.is_ok() {
                            return Ok(());
                        }
                        // Cleanup reports its own errors without killing the
                        // ACP connection (a spawned task error would do that).
                        let _ =
                            cleanup_terminal(&connection, &session_id, &terminal_id, true).await;
                    }
                    Err(error) => {
                        tracing::warn!(code = ?error.code, "terminal/create failed");
                        // No terminal was created; the turn may already be gone.
                        let _ = result_tx.send(Err(error));
                    }
                }
                Ok(())
            })?;
            let response = result_rx.await.map_err(|_| {
                agent_client_protocol::Error::internal_error().data("terminal connection closed")
            })??;
            accepted_tx.send(()).map_err(|()| {
                agent_client_protocol::Error::internal_error().data("terminal connection closed")
            })?;
            Ok(response)
        })
    }

    fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> AcpRequestFuture<'_, TerminalOutputResponse> {
        Box::pin(self.send_request(request).block_task())
    }

    fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> AcpRequestFuture<'_, WaitForTerminalExitResponse> {
        Box::pin(self.send_request(request).block_task())
    }

    fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> AcpRequestFuture<'_, ReleaseTerminalResponse> {
        Box::pin(self.send_request(request).block_task())
    }

    fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> AcpRequestFuture<'_, KillTerminalResponse> {
        Box::pin(self.send_request(request).block_task())
    }
}

/// Attempt all cleanup operations, with a one-second deadline per editor RPC.
pub(crate) async fn cleanup_terminal(
    requester: &dyn TerminalRequester,
    session_id: &SessionId,
    terminal_id: &TerminalId,
    kill: bool,
) -> Result<(), agent_client_protocol::Error> {
    let kill_result = if kill {
        bounded_terminal_cleanup(
            "terminal/kill",
            requester.kill_terminal(KillTerminalRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            )),
        )
        .await
        .map(|_| ())
    } else {
        Ok(())
    };
    // Even a failed or unanswered kill must not skip release.
    let release_result = bounded_terminal_cleanup(
        "terminal/release",
        requester.release_terminal(ReleaseTerminalRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        )),
    )
    .await;
    kill_result?;
    release_result?;
    Ok(())
}

async fn bounded_terminal_cleanup<T>(
    operation: &'static str,
    request: AcpRequestFuture<'_, T>,
) -> Result<T, agent_client_protocol::Error> {
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), request).await {
        if let Err(error) = &result {
            // Editor error payloads can contain command text or secrets.
            tracing::warn!(operation, code = ?error.code, "terminal cleanup failed");
        }
        result
    } else {
        tracing::warn!(
            operation,
            "terminal cleanup timed out; remote process state is unknown"
        );
        Err(agent_client_protocol::Error::internal_error().data("terminal cleanup timed out"))
    }
}

/// Reports that a tool call has stopped waiting and started working.
///
/// Unlike its sibling traits this sends a notification rather than asking the
/// client for anything, because the fact it conveys is one only the tool knows:
/// a call announced as `Pending` sits that way through the permission
/// round-trip, and the moment it stops waiting is inside the tool, after the
/// user has approved it. Reporting progress from the turn loop instead would
/// claim the work had started while the adapter was still blocked on the
/// approval dialog (daa-reep).
pub(crate) trait ToolProgressReporter: Send + Sync {
    /// Tell the client that `tool_call_id` is running now.
    fn report_in_progress(&self, session_id: &SessionId, tool_call_id: &str);
}

impl ToolProgressReporter for agent_client_protocol::ConnectionTo<Client> {
    fn report_in_progress(&self, session_id: &SessionId, tool_call_id: &str) {
        // Best effort. A client that has gone away will fail the tool call's
        // own notifications too, and the turn reports that; failing the command
        // because a progress hint did not land would be the wrong trade.
        let _ = self.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.to_string(),
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            )),
        ));
    }
}

pub(crate) trait ToolCallRequester:
    ReadTextFileRequester
    + WriteTextFileRequester
    + PermissionRequester
    + TerminalRequester
    + ToolProgressReporter
{
}

impl<T> ToolCallRequester for T where
    T: ReadTextFileRequester
        + WriteTextFileRequester
        + PermissionRequester
        + TerminalRequester
        + ToolProgressReporter
        + ?Sized
{
}

pub(crate) trait PermissionRequester: Send + Sync {
    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> AcpRequestFuture<'_, RequestPermissionResponse>;
}

// Production always speaks as the agent, so the tool layer is handed a
// `ConnectionTo<Client>` (see `serve_with_transport`). The `ConnectionTo<Agent>`
// direction exists only as a test seam: `read_file_tool_execution` is covered
// against a real client-backed connection (whose handle is a
// `ConnectionTo<Agent>`). Only `read_text_file` is needed for that seam, so the
// other requester traits are intentionally implemented for `ConnectionTo<Client>`
// alone.
impl ReadTextFileRequester for agent_client_protocol::ConnectionTo<Agent> {
    fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> AcpRequestFuture<'_, ReadTextFileResponse> {
        Box::pin(self.send_request(request).block_task())
    }
}

impl ReadTextFileRequester for agent_client_protocol::ConnectionTo<Client> {
    fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> AcpRequestFuture<'_, ReadTextFileResponse> {
        Box::pin(self.send_request(request).block_task())
    }
}

pub(crate) fn recover_null_write_response(
    result: Result<WriteTextFileResponse, agent_client_protocol::Error>,
) -> Result<WriteTextFileResponse, agent_client_protocol::Error> {
    result.or_else(|err| {
        let is_null_payload_deser_failure = err.code
            == agent_client_protocol::ErrorCode::ParseError
            && err.data.as_ref().is_some_and(|d| {
                d.get("json").is_some_and(serde_json::Value::is_null)
                    && d.get("phase").and_then(serde_json::Value::as_str) == Some("deserialization")
            });
        if is_null_payload_deser_failure {
            Ok(WriteTextFileResponse::new())
        } else {
            Err(err)
        }
    })
}

impl WriteTextFileRequester for agent_client_protocol::ConnectionTo<Client> {
    fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> AcpRequestFuture<'_, WriteTextFileResponse> {
        Box::pin(async move {
            recover_null_write_response(self.send_request(request).block_task().await)
        })
    }
}

impl PermissionRequester for agent_client_protocol::ConnectionTo<Client> {
    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> AcpRequestFuture<'_, RequestPermissionResponse> {
        Box::pin(self.send_request(request).block_task())
    }
}

use crate::SessionStore;
use crate::session::{
    PERMISSION_ALLOW_ALWAYS_OPTION_ID, PERMISSION_ALLOW_ONCE_OPTION_ID,
    PERMISSION_REJECT_ALWAYS_OPTION_ID, PERMISSION_REJECT_ONCE_OPTION_ID, PermissionDecision,
};
use crate::tools::ToolContext;
use crate::turn::tool_raw_input;
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::ToolCall as ChatToolCall;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
};
use tokio_util::sync::CancellationToken;

/// Ask the client (or fall back to posture) whether a tool call is allowed.
///
/// # Errors
///
/// Returns an [`AdapterError`] when the session is unknown, the permission request
/// cannot be sent, or the client returns an unrecognized outcome.
pub(crate) async fn request_tool_permission(
    store: &SessionStore,
    context: &ToolContext,
    call: &ChatToolCall,
    kind: crate::tools::ToolKind,
    requester: &dyn PermissionRequester,
    cancellation: &CancellationToken,
) -> Result<PermissionDecision, AdapterError> {
    if cancellation.is_cancelled() {
        return Ok(PermissionDecision::Cancelled);
    }
    if store.is_always_rejected(&context.session_id, call.name())? {
        return Ok(PermissionDecision::RejectAlways);
    }
    if store.is_always_allowed(&context.session_id, call.name())? {
        return Ok(PermissionDecision::AllowAlways);
    }

    let behavior = store.session_behavior(&context.session_id)?;

    if behavior.allows_without_prompt(kind) {
        return Ok(PermissionDecision::AllowByMode);
    }

    let request = RequestPermissionRequest::new(
        context.session_id.clone(),
        ToolCallUpdate::new(
            call.id().to_string(),
            ToolCallUpdateFields::new()
                .kind(agent_client_protocol::schema::v1::ToolKind::from(kind))
                .status(ToolCallStatus::Pending)
                .title(crate::turn::tool_call_title(call))
                .raw_input(tool_raw_input(call)),
        ),
        permission_options(),
    );

    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Ok(PermissionDecision::Cancelled),
        response = requester.request_permission(request) => response,
    }
    .map_err(|e| AdapterError::Internal(e.to_string()))?;
    // Approval and cancellation can become ready together. A stale reply must
    // neither authorize this call nor change remembered permission decisions.
    if cancellation.is_cancelled() {
        return Ok(PermissionDecision::Cancelled);
    }
    let decision = match response.outcome {
        RequestPermissionOutcome::Cancelled => PermissionDecision::Cancelled,
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            PERMISSION_ALLOW_ONCE_OPTION_ID => PermissionDecision::AllowOnce,
            PERMISSION_ALLOW_ALWAYS_OPTION_ID => PermissionDecision::AllowAlways,
            PERMISSION_REJECT_ONCE_OPTION_ID => PermissionDecision::RejectOnce,
            PERMISSION_REJECT_ALWAYS_OPTION_ID => PermissionDecision::RejectAlways,
            other => {
                return Err(AdapterError::InvalidParams(format!(
                    "unknown permission option selected: {other}"
                )));
            }
        },
        _ => {
            return Err(AdapterError::InvalidParams(
                "unsupported permission outcome variant".to_string(),
            ));
        }
    };

    if decision == PermissionDecision::AllowAlways {
        store.add_always_allow(&context.session_id, call.name().to_string())?;
    } else if decision == PermissionDecision::RejectAlways {
        store.add_always_reject(&context.session_id, call.name().to_string())?;
    }

    Ok(decision)
}

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PERMISSION_ALLOW_ONCE_OPTION_ID,
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PERMISSION_ALLOW_ALWAYS_OPTION_ID,
            "Allow always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PERMISSION_REJECT_ONCE_OPTION_ID,
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PERMISSION_REJECT_ALWAYS_OPTION_ID,
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Bind the editor connection to the registry's domain executor seam.
pub(crate) struct EditorTools<'a>(pub(crate) Option<&'a dyn ToolCallRequester>);

impl crate::tools::ToolExecutor for EditorTools<'_> {
    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        context: &'a ToolContext,
        store: &'a SessionStore,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, crate::tools::ToolExecution> {
        crate::tools::execution::execute_tools(call, context, store, self.0, cancellation)
    }
}

impl<T: ToolCallRequester> crate::tools::ToolExecutor for T {
    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        context: &'a ToolContext,
        store: &'a SessionStore,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, crate::tools::ToolExecution> {
        crate::tools::execution::execute_tools(call, context, store, Some(self), cancellation)
    }
}
