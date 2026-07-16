#![allow(clippy::indexing_slicing)]
use super::{ModelRequestSettings, handle_prompt_request, stream_model_turn};
use crate::acp::{
    CreateTerminalRequester, KillTerminalRequester, PermissionRequester, ReadTextFileRequester,
    ReleaseTerminalRequester, TerminalOutputRequester, ToolCallRequester,
    WaitForTerminalExitRequester, WriteTextFileRequester, handle_delete_session_request,
    handle_new_session_request, handle_set_session_config_option_request,
};
use crate::session::{DEFAULT_MAX_TURN_REQUESTS, ReasoningEffort, SessionBehavior, SessionStore};
use crate::test_store;
use crate::test_utils::FakePermissionRequester;
use crate::tools::{
    AdapterToolRegistry, EmptyToolRegistry, ToolContext, ToolEdit, ToolExecution, ToolRegistry,
};
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, DeleteSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    StopReason, ToolCallContent, ToolCallStatus, ToolKind,
};
use deepseek_acp_adapter::deepseek::{
    ChatMessage, ChatRequest, DeepSeekError, FinishReason, LlmClient, MessageRole, StreamEvent,
    ToolCall as DeepSeekToolCall, ToolCallDelta, ToolDefinition,
};
use deepseek_acp_adapter::error::AdapterError;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct FakeLlmClient {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    streams: Mutex<VecDeque<Vec<FakeStreamStep>>>,
}

impl FakeLlmClient {
    fn new(events: Vec<Result<StreamEvent, DeepSeekError>>) -> Self {
        Self::with_steps(events.into_iter().map(FakeStreamStep::Event).collect())
    }

    fn with_steps(steps: Vec<FakeStreamStep>) -> Self {
        Self::with_streams(vec![steps])
    }

    fn with_streams(streams: Vec<Vec<FakeStreamStep>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            streams: Mutex::new(VecDeque::from(streams)),
        }
    }

    fn requests(&self) -> Arc<Mutex<Vec<ChatRequest>>> {
        Arc::clone(&self.requests)
    }
}

impl LlmClient for FakeLlmClient {
    fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, DeepSeekError>>, DeepSeekError> {
        self.requests
            .lock()
            .map_err(|error| DeepSeekError::InvalidResponse(error.to_string()))?
            .push(request);
        let steps = self
            .streams
            .lock()
            .map_err(|error| DeepSeekError::InvalidResponse(error.to_string()))?
            .pop_front()
            .ok_or_else(|| {
                DeepSeekError::InvalidResponse(
                    "fake client stream was requested too many times".to_string(),
                )
            })?;

        Ok(Box::pin(stream::unfold(
            (VecDeque::from(steps), cancellation_token),
            |(mut steps, cancellation_token)| async move {
                let step = steps.pop_front()?;
                match step {
                    FakeStreamStep::Event(event) => Some((event, (steps, cancellation_token))),
                    FakeStreamStep::WaitForCancel => {
                        cancellation_token.cancelled().await;
                        None
                    }
                }
            },
        )))
    }
}

enum FakeStreamStep {
    Event(Result<StreamEvent, DeepSeekError>),
    WaitForCancel,
}

struct PendingLlmClient {
    started: Arc<Notify>,
}

impl PendingLlmClient {
    fn new(started: Arc<Notify>) -> Self {
        Self { started }
    }
}

impl LlmClient for PendingLlmClient {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _cancellation_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, DeepSeekError>>, DeepSeekError> {
        self.started.notify_one();
        Ok(Box::pin(stream::pending::<
            Result<StreamEvent, DeepSeekError>,
        >()))
    }
}

struct TransitionRequester {
    permission: FakePermissionRequester,
}

impl TransitionRequester {
    fn new(responses: Vec<RequestPermissionResponse>) -> Self {
        Self {
            permission: FakePermissionRequester::new(responses),
        }
    }

    fn requests(&self) -> Arc<Mutex<Vec<RequestPermissionRequest>>> {
        self.permission.requests()
    }
}

impl PermissionRequester for TransitionRequester {
    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> BoxFuture<'_, Result<RequestPermissionResponse, agent_client_protocol::Error>> {
        self.permission.request_permission(request)
    }
}

impl ReadTextFileRequester for TransitionRequester {
    fn read_text_file(
        &self,
        _request: agent_client_protocol::schema::v1::ReadTextFileRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::ReadTextFileResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected read_text_file request"))
        })
    }
}

impl WriteTextFileRequester for TransitionRequester {
    fn write_text_file(
        &self,
        _request: agent_client_protocol::schema::v1::WriteTextFileRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::WriteTextFileResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected write_text_file request"))
        })
    }
}

impl CreateTerminalRequester for TransitionRequester {
    fn create_terminal(
        &self,
        _request: agent_client_protocol::schema::v1::CreateTerminalRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::CreateTerminalResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected create_terminal request"))
        })
    }
}

impl TerminalOutputRequester for TransitionRequester {
    fn terminal_output(
        &self,
        _request: agent_client_protocol::schema::v1::TerminalOutputRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::TerminalOutputResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected terminal_output request"))
        })
    }
}

impl WaitForTerminalExitRequester for TransitionRequester {
    fn wait_for_terminal_exit(
        &self,
        _request: agent_client_protocol::schema::v1::WaitForTerminalExitRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::WaitForTerminalExitResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected wait_for_terminal_exit request"))
        })
    }
}

impl ReleaseTerminalRequester for TransitionRequester {
    fn release_terminal(
        &self,
        _request: agent_client_protocol::schema::v1::ReleaseTerminalRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::ReleaseTerminalResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected release_terminal request"))
        })
    }
}

impl KillTerminalRequester for TransitionRequester {
    fn kill_terminal(
        &self,
        _request: agent_client_protocol::schema::v1::KillTerminalRequest,
    ) -> BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::KillTerminalResponse,
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            Err(agent_client_protocol::Error::internal_error()
                .data("unexpected kill_terminal request"))
        })
    }
}

fn assert_plan_exit_prompt(request: &ChatRequest) {
    assert_eq!(request.messages()[0].role(), MessageRole::System);
    assert!(request.messages()[0].content().contains("Plan mode"));
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.name() == "exit_plan_mode")
    );
    assert!(
        !request
            .tools()
            .iter()
            .any(|tool| tool.name() == "write_file")
    );
    assert!(
        !request
            .tools()
            .iter()
            .any(|tool| tool.name() == "run_command")
    );
}

fn assert_exit_mode_transition_request(request: &RequestPermissionRequest) {
    assert_eq!(request.options.len(), 3);
    assert_eq!(
        request
            .options
            .iter()
            .map(|option| option.option_id.0.as_ref())
            .collect::<Vec<_>>(),
        vec!["ask", "accept-edits", "yolo"]
    );
}

fn assert_normal_request(request: &ChatRequest) {
    assert!(
        request
            .messages()
            .iter()
            .all(|message| message.role() != MessageRole::System)
    );
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.name() == "write_file")
    );
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.name() == "run_command")
    );
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.name() == "update_plan")
    );
}

struct FakeToolRegistry {
    definitions: Vec<ToolDefinition>,
    result: ToolExecution,
    calls: Arc<Mutex<Vec<DeepSeekToolCall>>>,
}

struct PlanModeToolRegistry {
    definitions: Vec<ToolDefinition>,
    calls: Arc<Mutex<Vec<DeepSeekToolCall>>>,
}

impl PlanModeToolRegistry {
    fn new() -> Self {
        let definitions = vec![
            ToolDefinition::new(
                "read_file",
                "Read a text file",
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "additionalProperties": false,
                }),
            ),
            ToolDefinition::new(
                "update_plan",
                "Update the current plan",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "entries": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string" },
                                    "priority": { "type": "string" },
                                    "status": { "type": "string" },
                                },
                                "required": ["content", "priority", "status"],
                                "additionalProperties": false,
                            },
                        },
                    },
                    "required": ["entries"],
                    "additionalProperties": false,
                }),
            ),
            ToolDefinition::new(
                "write_file",
                "Write a text file",
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "additionalProperties": false,
                }),
            ),
            ToolDefinition::new(
                "run_command",
                "Run a shell command",
                serde_json::json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                    "additionalProperties": false,
                }),
            ),
            ToolDefinition::new(
                "mcp__server__tool",
                "MCP tool",
                serde_json::json!({
                    "type": "object",
                    "properties": { "input": { "type": "string" } },
                    "required": ["input"],
                    "additionalProperties": false,
                }),
            ),
        ];

        Self {
            definitions,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Arc<Mutex<Vec<DeepSeekToolCall>>> {
        Arc::clone(&self.calls)
    }
}

impl ToolRegistry for PlanModeToolRegistry {
    fn definitions(
        &self,
        _context: &ToolContext,
        _store: &SessionStore,
    ) -> Result<Vec<ToolDefinition>, AdapterError> {
        Ok(self.definitions.clone())
    }

    fn kind(&self, name: &str) -> ToolKind {
        match name {
            "read_file" => ToolKind::Read,
            "update_plan" => ToolKind::Think,
            "write_file" => ToolKind::Edit,
            "run_command" => ToolKind::Execute,
            name if crate::is_mcp_tool_name(name) => crate::mcp_tool_kind(),
            _ => ToolKind::Other,
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a DeepSeekToolCall,
        _context: &'a ToolContext,
        _store: &'a SessionStore,
        _connection: Option<&'a dyn ToolCallRequester>,
        _cancellation_token: CancellationToken,
    ) -> BoxFuture<'a, ToolExecution> {
        Box::pin(async move {
            self.calls
                .lock()
                .map(|mut calls| calls.push(call.clone()))
                .ok();
            ToolExecution::failed("unexpected tool execution in plan mode")
        })
    }
}

impl FakeToolRegistry {
    fn new() -> Self {
        Self {
            definitions: vec![ToolDefinition::new(
                "echo",
                "Echo a message",
                serde_json::json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                }),
            )],
            result: ToolExecution::completed(
                "tool says hi",
                serde_json::json!({ "message": "tool says hi" }),
            ),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Arc<Mutex<Vec<DeepSeekToolCall>>> {
        Arc::clone(&self.calls)
    }
}

impl ToolRegistry for FakeToolRegistry {
    fn definitions(
        &self,
        _context: &ToolContext,
        _store: &SessionStore,
    ) -> Result<Vec<ToolDefinition>, AdapterError> {
        Ok(self.definitions.clone())
    }

    fn kind(&self, _name: &str) -> ToolKind {
        ToolKind::Other
    }

    fn execute<'a>(
        &'a self,
        call: &'a DeepSeekToolCall,
        _context: &'a ToolContext,
        _store: &'a SessionStore,
        _connection: Option<&'a dyn ToolCallRequester>,
        _cancellation_token: CancellationToken,
    ) -> BoxFuture<'a, ToolExecution> {
        Box::pin(async move {
            self.calls
                .lock()
                .map(|mut calls| calls.push(call.clone()))
                .ok();
            self.result.clone()
        })
    }
}

fn assert_diff_tool_update(
    notification: &SessionNotification,
) -> Result<(), agent_client_protocol::Error> {
    let SessionUpdate::ToolCallUpdate(update) = &notification.update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected tool call update")
        );
    };
    let Some(content) = &update.fields.content else {
        return Err(
            agent_client_protocol::Error::internal_error().data("missing tool call update content")
        );
    };
    let Some(ToolCallContent::Diff(diff)) = content.first() else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected diff tool call content")
        );
    };
    assert_eq!(diff.path, PathBuf::from("src/lib.rs"));
    assert_eq!(diff.old_text, Some("old text".to_string()));
    assert_eq!(diff.new_text, "new text");

    let Some(locations) = &update.fields.locations else {
        return Err(agent_client_protocol::Error::internal_error()
            .data("missing tool call update locations"));
    };
    let Some(location) = locations.first() else {
        return Err(
            agent_client_protocol::Error::internal_error().data("missing tool call location")
        );
    };
    assert_eq!(location.path, PathBuf::from("src/lib.rs"));
    assert_eq!(location.line, Some(7));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_uses_updated_session_model_and_reasoning()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    handle_set_session_config_option_request(
        &store,
        &SetSessionConfigOptionRequest::new(
            session.session_id.clone(),
            crate::SESSION_CONFIG_MODEL_ID,
            "deepseek-v4-flash",
        ),
    )?;
    handle_set_session_config_option_request(
        &store,
        &SetSessionConfigOptionRequest::new(
            session.session_id.clone(),
            crate::SESSION_CONFIG_REASONING_EFFORT_ID,
            "max",
        ),
    )?;

    let client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let requests = client.requests();

    let response = handle_prompt_request(
        &store,
        &client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("hi")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), 1);
    assert_eq!(request_guard[0].model(), Some("deepseek-v4-flash"));
    assert_eq!(request_guard[0].reasoning_effort(), Some("max"));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_uses_updated_session_max_tokens() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    handle_set_session_config_option_request(
        &store,
        &SetSessionConfigOptionRequest::new(
            session.session_id.clone(),
            crate::SESSION_CONFIG_MAX_TOKENS_ID,
            "8192",
        ),
    )?;

    let client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let requests = client.requests();

    handle_prompt_request(
        &store,
        &client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("hi")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard[0].max_tokens(), Some(8_192));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_omits_max_tokens_by_default() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;

    let client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let requests = client.requests();

    handle_prompt_request(
        &store,
        &client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("hi")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard[0].max_tokens(), None);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn plan_mode_injects_instructions_and_filters_mutating_tools()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    store.set_mode(&session.session_id, SessionBehavior::Plan)?;

    let client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let requests = client.requests();
    let registry = PlanModeToolRegistry::new();

    handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("make a plan")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), 1);
    let request = &request_guard[0];
    assert_eq!(request.messages().len(), 2);
    assert_eq!(request.messages()[0].role(), MessageRole::System);
    assert!(request.messages()[0].content().contains("Plan mode"));
    assert!(request.messages()[0].content().contains("update_plan"));
    assert_eq!(request.messages()[1].role(), MessageRole::User);
    assert_eq!(request.messages()[1].content(), "make a plan");

    let tool_names = request
        .tools()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["read_file", "update_plan"]);

    let state_guard = store
        .state
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let stored = state_guard
        .sessions
        .get(&session.session_id)
        .ok_or_else(|| {
            agent_client_protocol::Error::internal_error().data("missing stored session")
        })?;
    assert_eq!(stored.mode, SessionBehavior::Plan);
    assert_eq!(stored.history.len(), 2);
    assert_eq!(stored.history[0].role(), MessageRole::User);
    assert_eq!(stored.history[1].role(), MessageRole::Assistant);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn plan_mode_refuses_disallowed_tool_calls_before_execution()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    store.set_mode(&session.session_id, SessionBehavior::Plan)?;

    let client = FakeLlmClient::with_streams(vec![
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                0,
                Some("call-1".to_string()),
                Some("run_command".to_string()),
                Some(serde_json::json!({ "command": "echo hi" }).to_string()),
            )))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::ToolCalls))),
        ],
        vec![FakeStreamStep::Event(Ok(StreamEvent::Finished(
            FinishReason::EndTurn,
        )))],
    ]);
    let registry = PlanModeToolRegistry::new();
    let calls = registry.calls();

    let response = handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("make a plan")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let calls_guard = calls
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert!(calls_guard.is_empty());

    Ok(())
}

#[test_log::test(tokio::test)]
async fn leaving_plan_mode_restores_normal_request_assembly()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let client = FakeLlmClient::with_streams(vec![
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::Message("plan complete".to_string()))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
        ],
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::Message("work complete".to_string()))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
        ],
    ]);
    let requests = client.requests();
    let registry = PlanModeToolRegistry::new();

    store.set_mode(&session.session_id, SessionBehavior::Plan)?;
    handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("make a plan")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    store.set_mode(&session.session_id, SessionBehavior::Ask)?;
    handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("do the work")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), 2);
    assert_eq!(request_guard[0].messages().len(), 2);
    assert_eq!(request_guard[0].messages()[0].role(), MessageRole::System);
    assert!(
        request_guard[1]
            .messages()
            .iter()
            .all(|message| message.role() != MessageRole::System)
    );
    assert_eq!(
        request_guard[1].messages().last().map(ChatMessage::content),
        Some("do the work")
    );
    let tool_names = request_guard[1]
        .tools()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names,
        vec![
            "read_file".to_string(),
            "update_plan".to_string(),
            "write_file".to_string(),
            "run_command".to_string(),
            "mcp__server__tool".to_string(),
        ]
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn plan_mode_exit_transition_updates_mode_and_restores_normal_behavior()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    store.set_mode(&session.session_id, SessionBehavior::Plan)?;

    let client = FakeLlmClient::with_streams(vec![
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                0,
                Some("call-exit".to_string()),
                Some("exit_plan_mode".to_string()),
                Some("{}".to_string()),
            )))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::ToolCalls))),
        ],
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::Message("work complete".to_string()))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
        ],
    ]);
    let requests = client.requests();
    let requester = TransitionRequester::new(vec![RequestPermissionResponse::new(
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("ask")),
    )]);
    let requester_requests = requester.requests();
    let registry = AdapterToolRegistry;
    let mut notifications = Vec::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &registry,
        Some(&requester as &dyn ToolCallRequester),
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("make a plan")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert!(notifications.iter().any(|notification| matches!(
        notification.update,
        SessionUpdate::CurrentModeUpdate(ref update)
            if update.current_mode_id.0.as_ref() == "ask"
    )));

    {
        let request_guard = requests
            .lock()
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        assert_eq!(request_guard.len(), 1);
        assert_plan_exit_prompt(&request_guard[0]);
    }

    {
        let transition_guard = requester_requests
            .lock()
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        assert_eq!(transition_guard.len(), 1);
        assert_exit_mode_transition_request(&transition_guard[0]);
    }

    {
        let state_guard = store
            .state
            .lock()
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        let stored = state_guard
            .sessions
            .get(&session.session_id)
            .ok_or_else(|| {
                agent_client_protocol::Error::internal_error()
                    .data("missing stored session after transition")
            })?;
        assert_eq!(stored.mode, SessionBehavior::Ask);
    }

    handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("do the work")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    {
        let request_guard = requests
            .lock()
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        assert_eq!(request_guard.len(), 2);
        assert_normal_request(&request_guard[1]);
    }

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_streams_updates_and_stores_history() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let client = FakeLlmClient::new(vec![
        Ok(StreamEvent::Thought("thinking".to_string())),
        Ok(StreamEvent::Message("hello".to_string())),
        Ok(StreamEvent::Message(" world".to_string())),
        Ok(StreamEvent::Finished(FinishReason::EndTurn)),
    ]);
    let requests = client.requests();
    let mut notifications = Vec::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(session.session_id.clone(), vec![ContentBlock::from("hi")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(notifications.len(), 4);
    let SessionUpdate::SessionInfoUpdate(session_info_update) = &notifications[0].update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected session info update")
        );
    };
    assert!(session_info_update.title.is_value());
    assert!(session_info_update.updated_at.is_value());
    let SessionUpdate::AgentThoughtChunk(thought_chunk) = &notifications[1].update else {
        return Err(agent_client_protocol::Error::internal_error().data("expected thought chunk"));
    };
    let SessionUpdate::AgentMessageChunk(first_message_chunk) = &notifications[2].update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected first message chunk")
        );
    };
    let SessionUpdate::AgentMessageChunk(second_message_chunk) = &notifications[3].update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected second message chunk")
        );
    };
    assert!(thought_chunk.message_id.is_some());
    assert!(first_message_chunk.message_id.is_some());
    assert_eq!(
        first_message_chunk.message_id,
        second_message_chunk.message_id
    );
    assert_ne!(thought_chunk.message_id, first_message_chunk.message_id);

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), 1);
    assert_eq!(request_guard[0].messages()[0].content(), "hi");
    drop(request_guard);

    let state_guard = store
        .state
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let stored = state_guard
        .sessions
        .get(&session.session_id)
        .ok_or_else(|| {
            agent_client_protocol::Error::internal_error().data("missing stored session")
        })?;
    assert_eq!(stored.history.len(), 2);
    assert_eq!(stored.history[0].content(), "hi");
    assert_eq!(stored.history[1].content(), "hello world");

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_does_not_emit_plan_from_plain_text() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let mut notifications = Vec::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(
            session.session_id,
            vec![ContentBlock::from(
                "first sentence. second sentence. third sentence.",
            )],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(notifications.len(), 1);
    assert!(
        !notifications
            .iter()
            .any(|notification| matches!(notification.update, SessionUpdate::Plan(_)))
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_emits_explicit_plan_update_from_tool_call()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let client = FakeLlmClient::with_streams(vec![
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                0,
                Some("call-plan".to_string()),
                Some("update_plan".to_string()),
                Some(
                    serde_json::json!({
                        "entries": [
                            {
                                "content": "Inspect the failing tests",
                                "priority": "high",
                                "status": "in_progress",
                            },
                            {
                                "content": "Land the fix",
                                "priority": "medium",
                                "status": "pending",
                            },
                        ]
                    })
                    .to_string(),
                ),
            )))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::ToolCalls))),
        ],
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::Message("plan updated".to_string()))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
        ],
    ]);
    let mut notifications = Vec::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &AdapterToolRegistry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("make a plan")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert!(matches!(
        notifications[0].update,
        SessionUpdate::SessionInfoUpdate(_)
    ));
    assert!(matches!(
        notifications[1].update,
        SessionUpdate::ToolCall(_)
    ));
    assert!(matches!(
        notifications[2].update,
        SessionUpdate::ToolCallUpdate(_)
    ));
    let SessionUpdate::Plan(plan) = &notifications[3].update else {
        return Err(agent_client_protocol::Error::internal_error().data("expected plan update"));
    };
    assert_eq!(plan.entries.len(), 2);
    assert_eq!(plan.entries[0].content, "Inspect the failing tests");
    assert_eq!(
        plan.entries[0].priority,
        agent_client_protocol::schema::v1::PlanEntryPriority::High
    );
    assert_eq!(
        plan.entries[0].status,
        agent_client_protocol::schema::v1::PlanEntryStatus::InProgress
    );
    assert_eq!(plan.entries[1].content, "Land the fix");
    assert_eq!(
        plan.entries[1].priority,
        agent_client_protocol::schema::v1::PlanEntryPriority::Medium
    );
    assert_eq!(
        plan.entries[1].status,
        agent_client_protocol::schema::v1::PlanEntryStatus::Pending
    );
    assert!(matches!(
        notifications[4].update,
        SessionUpdate::AgentMessageChunk(_)
    ));
    assert!(
        notifications
            .iter()
            .any(|notification| matches!(notification.update, SessionUpdate::Plan(_)))
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_omits_unchanged_title_in_session_info_update()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;

    let first_client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    handle_prompt_request(
        &store,
        &first_client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("first prompt")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let second_client = FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::EndTurn))]);
    let mut notifications = Vec::new();
    let response = handle_prompt_request(
        &store,
        &second_client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(
            session.session_id,
            vec![ContentBlock::from("second prompt")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(notifications.len(), 1);
    let SessionUpdate::SessionInfoUpdate(session_info_update) = &notifications[0].update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected session info update")
        );
    };
    assert!(session_info_update.title.is_undefined());
    assert!(session_info_update.updated_at.is_value());

    Ok(())
}

#[test_log::test(tokio::test)]
async fn cancel_notification_stops_active_prompt() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let session_id = session.session_id.clone();
    let client = Arc::new(FakeLlmClient::with_steps(vec![
        FakeStreamStep::Event(Ok(StreamEvent::Message("partial".to_string()))),
        FakeStreamStep::WaitForCancel,
    ]));
    let (notification_tx, mut notification_rx) =
        tokio::sync::mpsc::unbounded_channel::<SessionNotification>();

    let prompt_store = store.clone();
    let prompt_session_id = session_id.clone();
    let prompt_client = Arc::clone(&client);
    let prompt_task = tokio::spawn(async move {
        handle_prompt_request(
            &prompt_store,
            prompt_client.as_ref(),
            &EmptyToolRegistry,
            None,
            PromptRequest::new(prompt_session_id, vec![ContentBlock::from("cancel me")]),
            DEFAULT_MAX_TURN_REQUESTS,
            |notification| {
                notification_tx
                    .send(notification)
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                Ok(())
            },
        )
        .await
    });

    let plan_notification = notification_rx.recv().await.ok_or_else(|| {
        agent_client_protocol::Error::internal_error().data("missing session info update")
    })?;
    let SessionUpdate::SessionInfoUpdate(session_info_update) = &plan_notification.update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected session info update")
        );
    };
    assert!(session_info_update.title.is_value());
    assert!(session_info_update.updated_at.is_value());

    let notification = notification_rx
        .recv()
        .await
        .ok_or_else(|| agent_client_protocol::Error::internal_error().data("missing update"))?;
    let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected agent message chunk")
        );
    };
    let ContentBlock::Text(text) = chunk.content else {
        return Err(agent_client_protocol::Error::internal_error().data("expected text chunk"));
    };
    assert_eq!(text.text, "partial");

    store.cancel_active_turn(&CancelNotification::new(session_id.clone()).session_id)?;
    let response = prompt_task
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)??;

    assert_eq!(response.stop_reason, StopReason::Cancelled);
    let guard = store
        .state
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let session = guard
        .sessions
        .get(&session_id)
        .ok_or_else(|| agent_client_protocol::Error::internal_error().data("missing session"))?;
    assert!(session.active_turn.is_none());
    assert!(session.history.is_empty());

    Ok(())
}

#[test_log::test(tokio::test)]
async fn delete_session_cancels_prompt_without_failing_cleanup()
-> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let session_id = session.session_id.clone();
    let started = Arc::new(Notify::new());
    let client = Arc::new(PendingLlmClient::new(Arc::clone(&started)));

    let prompt_store = store.clone();
    let prompt_session_id = session_id.clone();
    let prompt_client = Arc::clone(&client);
    let prompt_task = tokio::spawn(async move {
        handle_prompt_request(
            &prompt_store,
            prompt_client.as_ref(),
            &EmptyToolRegistry,
            None,
            PromptRequest::new(prompt_session_id, vec![ContentBlock::from("delete me")]),
            DEFAULT_MAX_TURN_REQUESTS,
            |_| Ok(()),
        )
        .await
    });

    started.notified().await;
    let delete_response =
        handle_delete_session_request(&store, &DeleteSessionRequest::new(session_id.clone()))?;
    assert_eq!(
        serde_json::to_value(&delete_response)
            .map_err(agent_client_protocol::Error::into_internal_error)?,
        serde_json::json!({})
    );

    let response = tokio::time::timeout(std::time::Duration::from_secs(1), prompt_task)
        .await
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))?
        .map_err(agent_client_protocol::Error::into_internal_error)??;

    assert_eq!(response.stop_reason, StopReason::Cancelled);

    let guard = store
        .state
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert!(!guard.sessions.contains_key(&session_id));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn stream_model_turn_respects_cancellation_token() -> Result<(), agent_client_protocol::Error>
{
    let started = Arc::new(Notify::new());
    let client = PendingLlmClient::new(Arc::clone(&started));
    let cancellation_token = CancellationToken::new();
    let task_token = cancellation_token.clone();
    let session_id = agent_client_protocol::schema::v1::SessionId::new("session-cancel");
    let messages: Vec<ChatMessage> = Vec::new();
    let tool_definitions: Vec<ToolDefinition> = Vec::new();

    let turn_task = tokio::spawn(async move {
        let mut notify = |_| Ok(());
        stream_model_turn(
            &client,
            &messages,
            &tool_definitions,
            ModelRequestSettings {
                model: "deepseek-v4-pro",
                reasoning_effort: Some(ReasoningEffort::High),
                max_tokens: None,
            },
            task_token,
            &session_id,
            &mut notify,
        )
        .await
    });

    started.notified().await;
    cancellation_token.cancel();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(1), turn_task)
        .await
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))?
        .map_err(agent_client_protocol::Error::into_internal_error)??;

    assert_eq!(turn.stop_reason, StopReason::Cancelled);
    assert_eq!(turn.assistant_text, "");
    assert!(turn.tool_calls.is_empty());

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_executes_tool_calls_and_replays_results() -> Result<(), agent_client_protocol::Error>
{
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let client = FakeLlmClient::with_streams(vec![
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                0,
                Some("call-1".to_string()),
                Some("echo".to_string()),
                Some("{\"message\":\"".to_string()),
            )))),
            FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                0,
                None,
                None,
                Some("hi\"}".to_string()),
            )))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::ToolCalls))),
        ],
        vec![
            FakeStreamStep::Event(Ok(StreamEvent::Message("done".to_string()))),
            FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
        ],
    ]);
    let requests = client.requests();
    let mut registry = FakeToolRegistry::new();
    registry.result.edit = Some(ToolEdit {
        path: PathBuf::from("src/lib.rs"),
        old_text: Some("old text".to_string()),
        new_text: "new text".to_string(),
        line: 7,
    });
    let tool_calls = registry.calls();
    let mut notifications = Vec::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("use tool")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |notification| {
            notifications.push(notification);
            Ok(())
        },
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let SessionUpdate::SessionInfoUpdate(session_info_update) = &notifications[0].update else {
        return Err(
            agent_client_protocol::Error::internal_error().data("expected session info update")
        );
    };
    assert!(session_info_update.title.is_value());
    assert!(session_info_update.updated_at.is_value());
    assert!(matches!(
        notifications[1].update,
        SessionUpdate::ToolCall(_)
    ));
    assert!(matches!(
        notifications[2].update,
        SessionUpdate::ToolCallUpdate(_)
    ));
    assert_diff_tool_update(&notifications[2])?;
    assert!(matches!(
        notifications[3].update,
        SessionUpdate::AgentMessageChunk(_)
    ));

    let tool_call_guard = tool_calls
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(tool_call_guard.len(), 1);
    assert_eq!(tool_call_guard[0].id(), "call-1");
    assert_eq!(tool_call_guard[0].name(), "echo");
    assert_eq!(tool_call_guard[0].arguments(), "{\"message\":\"hi\"}");
    drop(tool_call_guard);

    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), 2);
    assert_eq!(request_guard[0].tools().len(), 1);
    let replayed = request_guard[1].messages();
    assert_eq!(replayed.len(), 3);
    assert_eq!(replayed[0].content(), "use tool");
    assert_eq!(replayed[1].tool_calls()[0].id(), "call-1");
    assert_eq!(
        replayed[2].role(),
        deepseek_acp_adapter::deepseek::MessageRole::Tool
    );
    assert_eq!(replayed[2].tool_call_id(), Some("call-1"));
    assert_eq!(replayed[2].content(), "tool says hi");

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_tool_loop_stops_at_max_turn_requests() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let limit = DEFAULT_MAX_TURN_REQUESTS.get();
    let mut streams = (0..limit)
        .map(|index| {
            vec![
                FakeStreamStep::Event(Ok(StreamEvent::ToolCallDelta(ToolCallDelta::new(
                    0,
                    Some(format!("call-{index}")),
                    Some("echo".to_string()),
                    Some("{}".to_string()),
                )))),
                FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::ToolCalls))),
            ]
        })
        .collect::<Vec<_>>();
    streams.push(vec![
        FakeStreamStep::Event(Ok(StreamEvent::Message("done".to_string()))),
        FakeStreamStep::Event(Ok(StreamEvent::Finished(FinishReason::EndTurn))),
    ]);
    let client = FakeLlmClient::with_streams(streams);
    let requests = client.requests();
    let registry = FakeToolRegistry::new();

    let response = handle_prompt_request(
        &store,
        &client,
        &registry,
        None,
        PromptRequest::new(session.session_id.clone(), vec![ContentBlock::from("loop")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::MaxTurnRequests);
    let request_guard = requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    assert_eq!(request_guard.len(), limit);
    drop(request_guard);

    let guard = store
        .state
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let record = guard
        .sessions
        .get(&session.session_id)
        .ok_or_else(|| agent_client_protocol::Error::internal_error().data("missing session"))?;
    assert_eq!(record.history.len(), 1 + (limit * 2));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn prompt_replays_history_on_next_turn() -> Result<(), agent_client_protocol::Error> {
    let store = test_store();
    let session = handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let first_client = FakeLlmClient::new(vec![
        Ok(StreamEvent::Message("first answer".to_string())),
        Ok(StreamEvent::Finished(FinishReason::EndTurn)),
    ]);
    handle_prompt_request(
        &store,
        &first_client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::from("first")],
        ),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    let second_client =
        FakeLlmClient::new(vec![Ok(StreamEvent::Finished(FinishReason::MaxTokens))]);
    let second_requests = second_client.requests();
    let response = handle_prompt_request(
        &store,
        &second_client,
        &EmptyToolRegistry,
        None,
        PromptRequest::new(session.session_id, vec![ContentBlock::from("second")]),
        DEFAULT_MAX_TURN_REQUESTS,
        |_| Ok(()),
    )
    .await?;

    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    let request_guard = second_requests
        .lock()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let messages = request_guard[0].messages();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content(), "first");
    assert_eq!(messages[1].content(), "first answer");
    assert_eq!(messages[2].content(), "second");

    Ok(())
}

#[test_log::test(tokio::test)]
async fn report_tool_call_generates_correct_notification()
-> Result<(), agent_client_protocol::Error> {
    let session_id = agent_client_protocol::schema::v1::SessionId::new("report-test");
    let call = DeepSeekToolCall::new(
        "call-rtc",
        "write_file",
        serde_json::json!({"path": "f"}).to_string(),
    );
    let mut notifications = Vec::new();
    super::report_tool_call(
        &session_id,
        &mut |n| {
            notifications.push(n);
            Ok(())
        },
        &call,
        ToolKind::Edit,
    )?;
    assert_eq!(notifications.len(), 1);
    let SessionUpdate::ToolCall(ref tc) = notifications[0].update else {
        return Err(agent_client_protocol::Error::internal_error().data("expected ToolCall"));
    };
    assert_eq!(tc.tool_call_id.0.as_ref(), "call-rtc");
    assert_eq!(tc.status, ToolCallStatus::Pending);
    Ok(())
}

#[test_log::test(tokio::test)]
async fn report_tool_result_with_edit_generates_diff_and_location()
-> Result<(), agent_client_protocol::Error> {
    let session_id = agent_client_protocol::schema::v1::SessionId::new("report-result");
    let call = DeepSeekToolCall::new("call-rt", "write_file", "{}");
    let exec = ToolExecution {
        content: "ok".to_string(),
        raw_output: serde_json::json!({"x": 1}),
        success: true,
        edit: Some(ToolEdit {
            path: std::path::PathBuf::from("/tmp/f.txt"),
            old_text: Some("prev".to_string()),
            new_text: "next".to_string(),
            line: 3,
        }),
    };
    let mut notifications = Vec::new();
    super::report_tool_result(
        &session_id,
        &mut |n| {
            notifications.push(n);
            Ok(())
        },
        &call,
        &exec,
    )?;
    assert_eq!(notifications.len(), 1);
    let SessionUpdate::ToolCallUpdate(ref update) = notifications[0].update else {
        return Err(agent_client_protocol::Error::internal_error().data("expected ToolCallUpdate"));
    };
    assert_eq!(update.tool_call_id.0.as_ref(), "call-rt");
    assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    assert!(update.fields.locations.is_some());
    let Some(ref locations) = update.fields.locations else {
        return Err(agent_client_protocol::Error::internal_error().data("missing locations"));
    };
    assert_eq!(locations[0].path, std::path::PathBuf::from("/tmp/f.txt"));
    assert_eq!(locations[0].line, Some(3));
    // Diff content
    let Some(ref content) = update.fields.content else {
        return Err(agent_client_protocol::Error::internal_error().data("missing content"));
    };
    let Some(ToolCallContent::Diff(diff)) = content.first() else {
        return Err(agent_client_protocol::Error::internal_error().data("expected Diff"));
    };
    assert_eq!(diff.new_text, "next");
    assert_eq!(diff.old_text, Some("prev".to_string()));
    Ok(())
}

#[test]
fn helper_raw_input_and_finish_reason_cover_branches() {
    use agent_client_protocol::schema::v1::StopReason;
    use deepseek_acp_adapter::deepseek::FinishReason;

    let valid_raw_input = DeepSeekToolCall::new(
        "valid-raw-input",
        "echo",
        serde_json::json!({ "a": 1 }).to_string(),
    );
    assert_eq!(
        super::tool_raw_input(&valid_raw_input),
        serde_json::json!({ "a": 1 })
    );
    let invalid_raw_input = DeepSeekToolCall::new("invalid-raw-input", "echo", "not json");
    assert_eq!(
        super::tool_raw_input(&invalid_raw_input),
        serde_json::json!("not json")
    );

    assert_eq!(
        crate::stop_reason_from_finish(&FinishReason::EndTurn),
        StopReason::EndTurn
    );
    assert_eq!(
        crate::stop_reason_from_finish(&FinishReason::ToolCalls),
        StopReason::EndTurn
    );
    assert_eq!(
        crate::stop_reason_from_finish(&FinishReason::Other("rate_limit".to_string())),
        StopReason::EndTurn
    );
    assert_eq!(
        crate::stop_reason_from_finish(&FinishReason::MaxTokens),
        StopReason::MaxTokens
    );
    assert_eq!(
        crate::stop_reason_from_finish(&FinishReason::Refusal),
        StopReason::Refusal
    );
}

#[test]
fn tool_call_title_read_file() {
    let call = DeepSeekToolCall::new("c1", "read_file", r#"{"path":"src/lib.rs"}"#);
    assert_eq!(super::tool_call_title(&call), "Read: src/lib.rs");
}

#[test]
fn tool_call_title_write_file() {
    let call = DeepSeekToolCall::new("c2", "write_file", r#"{"path":"Cargo.toml"}"#);
    assert_eq!(super::tool_call_title(&call), "Write: Cargo.toml");
}

#[test]
fn tool_call_title_edit_file() {
    let call = DeepSeekToolCall::new("c3", "edit_file", r#"{"path":"src/main.rs"}"#);
    assert_eq!(super::tool_call_title(&call), "Edit: src/main.rs");
}

#[test]
fn tool_call_title_list_dir() {
    let call = DeepSeekToolCall::new("c4", "list_dir", r#"{"path":"src/"}"#);
    assert_eq!(super::tool_call_title(&call), "List: src/");
}

#[test]
fn tool_call_title_grep() {
    let call = DeepSeekToolCall::new("c5", "grep", r#"{"pattern":"fn main"}"#);
    assert_eq!(super::tool_call_title(&call), "Search: fn main");
}

#[test]
fn tool_call_title_glob() {
    let call = DeepSeekToolCall::new("c6", "glob", r#"{"pattern":"*.rs"}"#);
    assert_eq!(super::tool_call_title(&call), "Glob: *.rs");
}

#[test]
fn tool_call_title_run_command() {
    let call = DeepSeekToolCall::new("c7", "run_command", r#"{"command":"ls -la"}"#);
    assert_eq!(super::tool_call_title(&call), "ls -la");
}

#[test]
fn tool_call_title_run_command_complex() {
    let call = DeepSeekToolCall::new(
        "c8",
        "run_command",
        r#"{"command":"pwd && sed -n '1,220p' /home/user/file.txt"}"#,
    );
    assert_eq!(
        super::tool_call_title(&call),
        "pwd && sed -n '1,220p' /home/user/file.txt"
    );
}

#[test]
fn tool_call_title_fallback_to_name_when_no_known_args() {
    let call = DeepSeekToolCall::new("c9", "custom_tool", r#"{"foo":"bar"}"#);
    assert_eq!(super::tool_call_title(&call), "custom_tool");
}

#[test]
fn tool_call_title_fallback_to_name_when_invalid_json() {
    let call = DeepSeekToolCall::new("c10", "some_tool", "not json at all");
    assert_eq!(super::tool_call_title(&call), "some_tool");
}

#[test]
fn tool_call_title_prefers_command_over_path() {
    // Args with both command and path should use command (higher priority).
    let call = DeepSeekToolCall::new(
        "c11",
        "run_command",
        r#"{"command":"cargo build","path":"src/"}"#,
    );
    assert_eq!(super::tool_call_title(&call), "cargo build");
}

#[test]
fn tool_call_title_empty_string_filtered_out() {
    let call = DeepSeekToolCall::new("c12", "run_command", r#"{"command":""}"#);
    assert_eq!(super::tool_call_title(&call), "run_command");
}

#[test]
fn filter_messages_by_size_returns_unchanged_when_under_budget() {
    let messages = vec![
        ChatMessage::user("first"),
        ChatMessage::user("second"),
        ChatMessage::user("third"),
    ];
    assert_eq!(super::filter_messages_by_size(&messages, 1_000), messages);
}

#[test]
fn filter_messages_by_size_keeps_older_message_past_an_oversized_recent_one() {
    // Regression test for daa-wx1: a single oversized *recent* message must not
    // stop the walk-back before smaller, older messages get a chance to fit.
    let first = ChatMessage::user("first");
    let older_small = ChatMessage::user("keep me");
    let newest_huge = ChatMessage::user("x".repeat(1_000));
    let messages = vec![first.clone(), older_small.clone(), newest_huge];

    let filtered = super::filter_messages_by_size(&messages, 100);

    assert_eq!(filtered, vec![first, older_small]);
}

#[test]
fn filter_messages_by_size_drops_oversized_tool_call_unit_as_a_whole() {
    // An assistant message requesting tool calls and the tool results that
    // answer it must be dropped or kept together - never split, which would
    // otherwise produce a dangling tool call or an orphaned tool result.
    let first = ChatMessage::user("first");
    let older_small = ChatMessage::user("keep me");
    let assistant_call = ChatMessage::assistant_with_tool_calls(
        "checking",
        vec![DeepSeekToolCall::new(
            "call-1",
            "read_file",
            r#"{"path":"a"}"#,
        )],
    );
    let tool_result = ChatMessage::tool_result("call-1", "x".repeat(2_000));
    let messages = vec![
        first.clone(),
        older_small.clone(),
        assistant_call,
        tool_result,
    ];

    let filtered = super::filter_messages_by_size(&messages, 200);

    assert_eq!(filtered, vec![first, older_small]);
    assert!(filtered.iter().all(|m| m.tool_calls().is_empty()));
    assert!(filtered.iter().all(|m| m.role() != MessageRole::Tool));
}

#[test]
fn filter_messages_by_size_keeps_tool_call_unit_together_when_it_fits() {
    let first = ChatMessage::user("first");
    let assistant_call = ChatMessage::assistant_with_tool_calls(
        "checking",
        vec![DeepSeekToolCall::new(
            "call-1",
            "read_file",
            r#"{"path":"a"}"#,
        )],
    );
    let tool_result = ChatMessage::tool_result("call-1", "small result");
    let padding = ChatMessage::user("x".repeat(500));
    let messages = vec![
        first.clone(),
        padding,
        assistant_call.clone(),
        tool_result.clone(),
    ];

    let filtered = super::filter_messages_by_size(&messages, 250);

    assert_eq!(filtered, vec![first, assistant_call, tool_result]);
}

#[test]
fn validate_tool_results_drops_orphaned_tool_result() {
    let messages = vec![
        ChatMessage::user("hello"),
        ChatMessage::tool_result("missing-call", "orphaned"),
    ];

    let validated = super::validate_tool_results(&messages);

    assert_eq!(validated, vec![messages[0].clone()]);
}

#[test]
fn sanitize_conversation_merges_consecutive_users() {
    // Regression test for daa-rts: truncation drops the assistant turns between
    // user messages, leaving a run of consecutive users that DeepSeek 400s on.
    let messages = vec![
        ChatMessage::user("first"),
        ChatMessage::user("ok"),
        ChatMessage::user("continue"),
    ];

    let sanitized = super::sanitize_conversation(messages);

    assert_eq!(
        sanitized,
        vec![ChatMessage::user("first\n\nok\n\ncontinue")]
    );
}

#[test]
fn sanitize_conversation_drops_empty_assistant_and_merges_around_it() {
    // Regression test for daa-rts: an empty assistant message serializes to
    // `{"role":"assistant"}` (no content, no tool calls) and is rejected. After
    // dropping it, the two surrounding users must also coalesce.
    let messages = vec![
        ChatMessage::user("before"),
        ChatMessage::assistant(""),
        ChatMessage::user("after"),
    ];

    let sanitized = super::sanitize_conversation(messages);

    assert_eq!(sanitized, vec![ChatMessage::user("before\n\nafter")]);
}

#[test]
fn sanitize_conversation_preserves_tool_calls_and_results() {
    // Tool messages and assistant-with-tool-calls must never be merged or
    // dropped, so tool-call/result pairing stays intact. Parallel tool results
    // (consecutive tool messages) are a valid shape and left untouched.
    let assistant_call = ChatMessage::assistant_with_tool_calls(
        "checking",
        vec![
            DeepSeekToolCall::new("call-1", "read_file", r#"{"path":"a"}"#),
            DeepSeekToolCall::new("call-2", "read_file", r#"{"path":"b"}"#),
        ],
    );
    let messages = vec![
        ChatMessage::user("go"),
        assistant_call.clone(),
        ChatMessage::tool_result("call-1", "content a"),
        ChatMessage::tool_result("call-2", "content b"),
    ];

    let sanitized = super::sanitize_conversation(messages.clone());

    assert_eq!(sanitized, messages);
}

#[test]
fn sanitize_conversation_leaves_valid_alternation_unchanged() {
    let messages = vec![
        ChatMessage::user("hi"),
        ChatMessage::assistant("hello"),
        ChatMessage::user("bye"),
    ];

    let sanitized = super::sanitize_conversation(messages.clone());

    assert_eq!(sanitized, messages);
}
