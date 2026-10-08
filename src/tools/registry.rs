//! Tool registry, context types, and execution results.

use std::path::PathBuf;

pub(crate) use crate::session::{ToolContext, ToolKind};
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::{ToolCall as ChatToolCall, ToolDefinition};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Executor bound to a turn's external tool services.
pub(crate) trait ToolExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        context: &'a ToolContext,
        store: &'a crate::SessionStore,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'a>;
}

type ToolExecutionFuture<'a> = BoxFuture<'a, ToolExecution>;

/// Registry for tools the model can call during a turn.
pub(crate) trait ToolRegistry: Send + Sync {
    /// Return tool definitions to advertise to the model.
    fn definitions(
        &self,
        context: &ToolContext,
        store: &crate::SessionStore,
    ) -> Result<Vec<ToolDefinition>, AdapterError>;

    /// Return the domain category used by planning and permission policy.
    fn kind(&self, name: &str) -> ToolKind;

    /// Execute a complete model-requested tool call.
    ///
    /// The `cancellation_token` is cancelled when the turn is cancelled (via
    /// `session/cancel`); long-running tools (e.g. terminal commands) should race
    /// their work against it and abort promptly.
    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        context: &'a ToolContext,
        store: &'a crate::SessionStore,
        executor: Option<&'a dyn ToolExecutor>,
        cancellation_token: CancellationToken,
    ) -> ToolExecutionFuture<'a>;
}

#[derive(Debug)]
#[cfg(test)]
pub(crate) struct EmptyToolRegistry;

#[cfg(test)]
impl ToolRegistry for EmptyToolRegistry {
    fn definitions(
        &self,
        _context: &ToolContext,
        _store: &crate::SessionStore,
    ) -> Result<Vec<ToolDefinition>, AdapterError> {
        Ok(Vec::new())
    }

    fn kind(&self, _name: &str) -> ToolKind {
        ToolKind::Other
    }

    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        _context: &'a ToolContext,
        _store: &'a crate::SessionStore,
        _executor: Option<&'a dyn ToolExecutor>,
        _cancellation_token: CancellationToken,
    ) -> ToolExecutionFuture<'a> {
        Box::pin(async move { ToolExecution::failed(format!("unknown tool: {}", call.name())) })
    }
}

#[derive(Debug)]
pub(crate) struct AdapterToolRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolExecution {
    pub(crate) content: String,
    pub(crate) raw_output: Value,
    pub(crate) success: bool,
    pub(crate) edit: Option<ToolEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolEdit {
    pub(crate) path: PathBuf,
    pub(crate) old_text: Option<String>,
    pub(crate) new_text: String,
    pub(crate) line: u32,
}

impl ToolExecution {
    #[cfg(test)]
    pub(crate) fn completed(content: impl Into<String>, raw_output: Value) -> Self {
        Self {
            content: content.into(),
            raw_output,
            success: true,
            edit: None,
        }
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            content: message.clone(),
            raw_output: serde_json::json!({ "error": message }),
            success: false,
            edit: None,
        }
    }

    pub(crate) fn content_for_model(&self) -> &str {
        &self.content
    }
}

#[cfg(test)]
// Test assertions legitimately use indexing to access elements by position; replacing
// every `slice[i]` with `.get(i).unwrap()` adds noise without safety benefit in tests.
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::acp::handle_new_session_request;
    use crate::session::PERMISSION_ALLOW_ONCE_OPTION_ID;
    use crate::test_store;
    use acp_llm_adapter::error::AdapterError;
    use acp_llm_adapter::llm::ToolCall as ChatToolCall;
    use agent_client_protocol::schema::v1::{
        ClientCapabilities, CreateTerminalRequest, CreateTerminalResponse, FileSystemCapabilities,
        KillTerminalRequest, KillTerminalResponse, NewSessionRequest, ReadTextFileRequest,
        ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SelectedPermissionOutcome, TerminalExitStatus, TerminalId, TerminalOutputRequest,
        TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
        WriteTextFileRequest, WriteTextFileResponse,
    };
    use futures_util::future::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    #[derive(Debug, Default)]
    struct RecordingToolCallRequester {
        cancel_on_permission: Option<CancellationToken>,
        hold_permission: bool,
        cancel_on_read: Option<CancellationToken>,
        read_file: AtomicUsize,
        write_file: AtomicUsize,
        permission: AtomicUsize,
        terminal_create: AtomicUsize,
        terminal_output: AtomicUsize,
        terminal_wait: AtomicUsize,
        terminal_release: AtomicUsize,
        progress: AtomicUsize,
    }

    impl RecordingToolCallRequester {
        fn read_calls(&self) -> usize {
            self.read_file.load(Ordering::SeqCst)
        }

        fn write_calls(&self) -> usize {
            self.write_file.load(Ordering::SeqCst)
        }

        fn permission_calls(&self) -> usize {
            self.permission.load(Ordering::SeqCst)
        }
    }

    impl crate::ReadTextFileRequester for RecordingToolCallRequester {
        fn read_text_file(
            &self,
            _request: ReadTextFileRequest,
        ) -> BoxFuture<'_, Result<ReadTextFileResponse, agent_client_protocol::Error>> {
            self.read_file.fetch_add(1, Ordering::SeqCst);
            if let Some(token) = &self.cancel_on_read {
                token.cancel();
            }
            Box::pin(async move { Ok(ReadTextFileResponse::new("client original")) })
        }
    }

    impl crate::WriteTextFileRequester for RecordingToolCallRequester {
        fn write_text_file(
            &self,
            _request: WriteTextFileRequest,
        ) -> BoxFuture<'_, Result<WriteTextFileResponse, agent_client_protocol::Error>> {
            self.write_file.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(WriteTextFileResponse::new()) })
        }
    }

    impl crate::PermissionRequester for RecordingToolCallRequester {
        fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> BoxFuture<'_, Result<RequestPermissionResponse, agent_client_protocol::Error>>
        {
            self.permission.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if let Some(token) = &self.cancel_on_permission {
                    token.cancel();
                    if self.hold_permission {
                        return std::future::pending().await;
                    }
                    return Ok(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            crate::session::PERMISSION_ALLOW_ALWAYS_OPTION_ID,
                        )),
                    ));
                }
                Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        PERMISSION_ALLOW_ONCE_OPTION_ID,
                    )),
                ))
            })
        }
    }

    impl crate::ToolProgressReporter for RecordingToolCallRequester {
        fn report_in_progress(
            &self,
            _session_id: &agent_client_protocol::schema::v1::SessionId,
            _tool_call_id: &str,
        ) {
            self.progress.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl crate::acp::TerminalRequester for RecordingToolCallRequester {
        fn create_terminal(
            &self,
            _request: CreateTerminalRequest,
        ) -> BoxFuture<'_, Result<CreateTerminalResponse, agent_client_protocol::Error>> {
            self.terminal_create.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(CreateTerminalResponse::new(TerminalId::new(
                    "registry-terminal",
                )))
            })
        }

        fn terminal_output(
            &self,
            _request: TerminalOutputRequest,
        ) -> BoxFuture<'_, Result<TerminalOutputResponse, agent_client_protocol::Error>> {
            self.terminal_output.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(TerminalOutputResponse::new("terminal output", false)) })
        }

        fn wait_for_terminal_exit(
            &self,
            _request: WaitForTerminalExitRequest,
        ) -> BoxFuture<'_, Result<WaitForTerminalExitResponse, agent_client_protocol::Error>>
        {
            self.terminal_wait.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(Some(0)),
                ))
            })
        }

        fn release_terminal(
            &self,
            _request: ReleaseTerminalRequest,
        ) -> BoxFuture<'_, Result<ReleaseTerminalResponse, agent_client_protocol::Error>> {
            self.terminal_release.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(ReleaseTerminalResponse::new()) })
        }

        fn kill_terminal(
            &self,
            _request: KillTerminalRequest,
        ) -> BoxFuture<'_, Result<KillTerminalResponse, agent_client_protocol::Error>> {
            Box::pin(async move { Ok(KillTerminalResponse::new()) })
        }
    }

    fn registry_context(cwd: std::path::PathBuf) -> ToolContext {
        ToolContext {
            session_id: agent_client_protocol::schema::v1::SessionId::new("session-registry-test")
                .to_string(),
            cwd,
            additional_directories: Vec::new(),
            client_capabilities: None,
        }
    }

    async fn check_cancelled_mutations(
        cancel_before: bool,
        hold_permission: bool,
        cancel_on_read: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("tool-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let file = root.join("sample.txt");
        for client_io in [false, true] {
            for (tool, arguments) in [
                (
                    "write_file",
                    serde_json::json!({"path":"sample.txt", "content":"changed"}),
                ),
                (
                    "edit_file",
                    serde_json::json!({"path":"sample.txt", "old_text":"original", "new_text":"changed"}),
                ),
                (
                    "run_command",
                    serde_json::json!({"command":"printf changed > sample.txt"}),
                ),
            ] {
                if cancel_on_read && (!client_io || tool == "run_command") {
                    continue;
                }
                std::fs::write(&file, "client original")?;
                let store = test_store();
                let session = handle_new_session_request(&store, &NewSessionRequest::new(&root))?;
                let mut context = registry_context(root.clone());
                context.session_id = session.session_id.0.to_string();
                if client_io {
                    context.client_capabilities = Some(
                        (ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new()
                                .read_text_file(true)
                                .write_text_file(true))
                            .terminal(true))
                        .into(),
                    );
                }
                let token = CancellationToken::new();
                if cancel_before {
                    store.set_mode(&context.session_id, crate::SessionBehavior::Yolo)?;
                    token.cancel();
                }
                let requester = RecordingToolCallRequester {
                    cancel_on_permission: (!cancel_before && !cancel_on_read)
                        .then(|| token.clone()),
                    hold_permission,
                    cancel_on_read: cancel_on_read.then(|| token.clone()),
                    ..RecordingToolCallRequester::default()
                };
                let call = ChatToolCall::new("cancelled-call", tool, arguments.to_string());
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    AdapterToolRegistry.execute(&call, &context, &store, Some(&requester), token),
                )
                .await?;
                assert!(
                    !result.success && result.content.contains("cancelled"),
                    "{tool}: {result:?}"
                );
                assert_eq!(
                    requester.write_calls(),
                    0,
                    "{tool} wrote after cancellation"
                );
                assert_eq!(requester.terminal_create.load(Ordering::SeqCst), 0);
                assert_eq!(requester.progress.load(Ordering::SeqCst), 0);
                assert_eq!(std::fs::read_to_string(&file)?, "client original");
                assert!(!store.is_always_allowed(&context.session_id, tool)?);
                if cancel_before {
                    assert_eq!(requester.permission_calls(), 0);
                }
            }
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn pending_builtin_approval_cancels_without_side_effects()
    -> Result<(), Box<dyn std::error::Error>> {
        check_cancelled_mutations(false, true, false).await
    }

    #[test_log::test(tokio::test)]
    async fn late_builtin_approval_neither_executes_nor_remembers_permission()
    -> Result<(), Box<dyn std::error::Error>> {
        check_cancelled_mutations(false, false, false).await
    }

    #[test_log::test(tokio::test)]
    async fn already_cancelled_builtin_tools_never_start() -> Result<(), Box<dyn std::error::Error>>
    {
        check_cancelled_mutations(true, false, false).await
    }

    #[test_log::test(tokio::test)]
    async fn cancellation_during_edit_preflight_prevents_writing()
    -> Result<(), Box<dyn std::error::Error>> {
        check_cancelled_mutations(false, false, true).await
    }

    #[test]
    fn empty_registry_definitions_returns_empty() -> Result<(), AdapterError> {
        let registry = EmptyToolRegistry;
        let context = registry_context(std::path::PathBuf::from("/tmp"));
        let store = test_store();
        let definitions = registry.definitions(&context, &store)?;
        assert!(definitions.is_empty());
        Ok(())
    }

    #[test]
    fn empty_registry_kind_returns_other() {
        let registry = EmptyToolRegistry;
        assert_eq!(registry.kind("anything"), ToolKind::Other);
    }

    #[test]
    fn adapter_registry_definitions_include_plan_tools() -> Result<(), AdapterError> {
        let registry = AdapterToolRegistry;
        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new("/tmp"))?;
        store.set_mode(&session.session_id.0, crate::SessionBehavior::Plan)?;
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            additional_directories: Vec::new(),
            client_capabilities: None,
        };
        let definitions = registry.definitions(&context, &store)?;

        assert!(
            definitions
                .iter()
                .any(|definition| definition.name() == "update_plan")
        );
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name() == "exit_plan_mode")
        );
        assert_eq!(registry.kind("update_plan"), ToolKind::Think);
        assert_eq!(registry.kind("exit_plan_mode"), ToolKind::Think);
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn empty_registry_execute_returns_failed() {
        let registry = EmptyToolRegistry;
        let context = registry_context(std::path::PathBuf::from("/tmp"));
        let store = test_store();
        let call = ChatToolCall::new("empty-call", "test_tool", "{}");
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        assert!(!result.success);
        assert!(result.content.contains("unknown tool: test_tool"));
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_read_file_local() -> Result<(), AdapterError> {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;
        std::fs::write(temp_root.join("sample.txt"), "alpha\nbeta\ngamma\n")
            .map_err(AdapterError::from)?;

        let registry = AdapterToolRegistry;
        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;
        let mut context = registry_context(temp_root.clone());
        context.session_id = session.session_id.0.to_string();
        let call = ChatToolCall::new(
            "reg-read",
            "read_file",
            serde_json::json!({"path": "sample.txt"}).to_string(),
        );
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        assert!(result.success);
        assert_eq!(result.content, "alpha\nbeta\ngamma");
        assert_eq!(result.raw_output["source"], "local");
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_client_file_tools_use_connection() -> Result<(), AdapterError>
    {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-client-fs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;

        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;
        let requester = RecordingToolCallRequester::default();
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: temp_root,
            additional_directories: Vec::new(),
            client_capabilities: Some(
                (ClientCapabilities::new().fs(FileSystemCapabilities::new()
                    .read_text_file(true)
                    .write_text_file(true)))
                .into(),
            ),
        };
        let registry = AdapterToolRegistry;

        let read_call = ChatToolCall::new(
            "reg-client-read",
            "read_file",
            serde_json::json!({"path": "sample.txt"}).to_string(),
        );
        let read_result = registry
            .execute(
                &read_call,
                &context,
                &store,
                Some(&requester),
                CancellationToken::new(),
            )
            .await;
        assert!(read_result.success);
        assert_eq!(read_result.content, "client original");
        assert_eq!(read_result.raw_output["source"], "client");

        let write_call = ChatToolCall::new(
            "reg-client-write",
            "write_file",
            serde_json::json!({"path": "sample.txt", "content": "replacement"}).to_string(),
        );
        let write_result = registry
            .execute(
                &write_call,
                &context,
                &store,
                Some(&requester),
                CancellationToken::new(),
            )
            .await;
        assert!(write_result.success);
        assert_eq!(write_result.raw_output["source"], "client");

        let edit_call = ChatToolCall::new(
            "reg-client-edit",
            "edit_file",
            serde_json::json!({
                "path": "sample.txt",
                "old_text": "original",
                "new_text": "edited"
            })
            .to_string(),
        );
        let edit_result = registry
            .execute(
                &edit_call,
                &context,
                &store,
                Some(&requester),
                CancellationToken::new(),
            )
            .await;
        assert!(edit_result.success);
        assert_eq!(edit_result.raw_output["read_source"], "client");
        assert_eq!(edit_result.raw_output["write_source"], "client");
        assert_eq!(requester.read_calls(), 3);
        assert_eq!(requester.write_calls(), 2);
        assert_eq!(requester.permission_calls(), 2);
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_write_file_local_no_permission() -> Result<(), AdapterError> {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;

        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;

        let registry = AdapterToolRegistry;
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: temp_root.clone(),
            additional_directories: Vec::new(),
            client_capabilities: None,
        };
        let call = ChatToolCall::new(
            "reg-write",
            "write_file",
            serde_json::json!({"path": "out.txt", "content": "hello world"}).to_string(),
        );
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        // write_file requires permission which is denied without a requester
        assert!(!result.success);
        assert!(result.content.contains("requires a client connection"));
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_edit_file_local_no_permission() -> Result<(), AdapterError> {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;
        std::fs::write(temp_root.join("source.txt"), "original content\n")
            .map_err(AdapterError::from)?;

        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;

        let registry = AdapterToolRegistry;
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: temp_root.clone(),
            additional_directories: Vec::new(),
            client_capabilities: None,
        };
        let call = ChatToolCall::new(
            "reg-edit",
            "edit_file",
            serde_json::json!({
                "path": "source.txt",
                "old_text": "original",
                "new_text": "modified"
            })
            .to_string(),
        );
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        // edit_file requires permission which is denied without a requester
        assert!(!result.success);
        assert!(result.content.contains("requires a client connection"));
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_run_command_local_no_permission() -> Result<(), AdapterError>
    {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-cmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;

        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;

        let registry = AdapterToolRegistry;
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: temp_root.clone(),
            additional_directories: Vec::new(),
            client_capabilities: None,
        };
        let call = ChatToolCall::new(
            "reg-cmd",
            "run_command",
            serde_json::json!({"command": "echo hello"}).to_string(),
        );
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        // run_command requires permission which is denied without a requester
        assert!(!result.success);
        assert!(result.content.contains("requires a client connection"));
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_run_command_uses_terminal_connection()
    -> Result<(), AdapterError> {
        let temp_root =
            std::env::temp_dir().join(format!("acp-llm-reg-terminal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_root).map_err(AdapterError::from)?;

        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&temp_root))?;
        let requester = RecordingToolCallRequester::default();
        let context = ToolContext {
            session_id: session.session_id.0.to_string(),
            cwd: temp_root,
            additional_directories: Vec::new(),
            client_capabilities: Some(
                (ClientCapabilities::new()
                    .terminal(true)
                    .fs(FileSystemCapabilities::new()))
                .into(),
            ),
        };
        let call = ChatToolCall::new(
            "reg-terminal",
            "run_command",
            serde_json::json!({"command": "echo via-terminal"}).to_string(),
        );

        let result = AdapterToolRegistry
            .execute(
                &call,
                &context,
                &store,
                Some(&requester),
                CancellationToken::new(),
            )
            .await;

        assert!(result.success);
        assert!(result.content.contains("terminal output"));
        assert_eq!(requester.permission_calls(), 1);
        assert_eq!(requester.terminal_create.load(Ordering::SeqCst), 1);
        assert_eq!(requester.terminal_wait.load(Ordering::SeqCst), 1);
        assert_eq!(requester.terminal_output.load(Ordering::SeqCst), 1);
        assert_eq!(requester.terminal_release.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn adapter_registry_execute_bogus_tool() -> Result<(), AdapterError> {
        let registry = AdapterToolRegistry;
        let store = test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new("/tmp"))?;
        let mut context = registry_context(std::path::PathBuf::from("/tmp"));
        context.session_id = session.session_id.0.to_string();
        let call = ChatToolCall::new("bogus-call", "no_such_tool", "{}");
        let result = registry
            .execute(&call, &context, &store, None, CancellationToken::new())
            .await;
        assert!(!result.success);
        assert!(result.content.contains("unknown tool: no_such_tool"));
        Ok(())
    }

    #[test]
    fn tool_execution_completed_constructs_correctly() {
        let exec = ToolExecution::completed("done", serde_json::json!({"ok": true}));
        assert!(exec.success);
        assert_eq!(exec.content, "done");
        assert_eq!(exec.raw_output, serde_json::json!({"ok": true}));
        assert!(exec.edit.is_none());
        assert!(exec.success);
        assert_eq!(exec.content_for_model(), "done");
    }

    #[test]
    fn tool_execution_failed_constructs_correctly() {
        let exec = ToolExecution::failed("error message");
        assert!(!exec.success);
        assert_eq!(exec.content, "error message");
        assert_eq!(
            exec.raw_output,
            serde_json::json!({"error": "error message"})
        );
        assert!(exec.edit.is_none());
        assert!(!exec.success);
        assert_eq!(exec.content_for_model(), "error message");
    }

    #[test]
    fn tool_execution_completed_result_is_successful() {
        let exec = ToolExecution {
            content: String::new(),
            raw_output: serde_json::Value::Null,
            success: true,
            edit: None,
        };
        assert!(exec.success);
    }

    #[test]
    fn tool_execution_failed_result_is_unsuccessful() {
        let exec = ToolExecution {
            content: String::new(),
            raw_output: serde_json::Value::Null,
            success: false,
            edit: None,
        };
        assert!(!exec.success);
    }

    #[test]
    fn tool_execution_content_for_model_returns_content_ref() {
        let exec = ToolExecution {
            content: "the response".to_string(),
            raw_output: serde_json::Value::Null,
            success: true,
            edit: None,
        };
        assert_eq!(exec.content_for_model(), "the response");
    }
}
