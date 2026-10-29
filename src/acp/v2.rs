//! Draft ACP v2 agent, negotiated alongside v1 (`protocol-v2` feature).
//!
//! `serve` routes each connection by its `initialize` request: v1 clients reach
//! the stable implementation in the parent module, v2 clients reach this one.
//! The v2 wire surface is translated here, at the edge, onto the same session
//! store, v1 session handlers and turn loop, so validation, persistence and
//! confinement are not duplicated.
//!
//! Implements the v2 session baseline (new, list, resume, close, prompt, cancel
//! and updates), config options, MCP and additional roots. v2 has no client
//! file system or terminal delegation, so built-in tools always run
//! locally; permission prompts go through the shared v1 gating logic and are
//! translated to v2 `session/request_permission` at this edge.

use std::num::NonZeroUsize;
use std::sync::Arc;

use self::tools::V2Tools;
use super::EditorTools;
use crate::session_store::SessionStore;
use crate::tools::ToolRegistry;
use crate::turn::{PromptInput, PromptResult, TurnEvent};
use crate::{ADAPTER_NAME, ADAPTER_VERSION, adapter_available_commands};
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::{LlmClient, MessageRole};
use agent_client_protocol::schema::v1;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, V2ConnectionTo};

mod diff;
mod tools;

type AcpResult<T> = Result<T, agent_client_protocol::Error>;

/// Everything a v2 connection shares with v1: one store, client and registry.
#[derive(Clone)]
pub(crate) struct Services {
    pub(crate) store: SessionStore,
    pub(crate) llm_client: Arc<dyn LlmClient>,
    pub(crate) tool_registry: Arc<dyn ToolRegistry>,
    pub(crate) max_turn_requests: NonZeroUsize,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Services")
            .field("max_turn_requests", &self.max_turn_requests)
            .finish_non_exhaustive()
    }
}

/// The v2 implementation handed to `Agent.protocol_router()`.
pub(crate) fn agent(services: Services) -> impl ConnectTo<Client> {
    let (new, list, resume, close, prompt, config, cancel) = (
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services,
    );
    Agent
        .v2()
        .name(ADAPTER_NAME)
        .on_receive_request(
            async |_request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                let services = new.clone();
                connection.clone().spawn(async move {
                    match new_session(&services.store, &request).await {
                        Ok(response) => {
                            let session_id = response.session_id.clone();
                            responder.respond(response)?;
                            send(&connection, &session_id, idle(None))
                        }
                        Err(error) => responder.respond_with_error(error),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ListSessionsRequest,
                        responder: Responder<v2::ListSessionsResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond_with_result(list_sessions(&list.store, &request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ResumeSessionRequest,
                        responder: Responder<v2::ResumeSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                let services = resume.clone();
                connection.clone().spawn(async move {
                    let result = resume_session(&services.store, &request, &connection).await;
                    responder.respond_with_result(result)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::CloseSessionRequest,
                        responder: Responder<v2::CloseSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond_with_result(close_session(&close.store, &request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::PromptRequest,
                        responder: Responder<v2::PromptResponse>,
                        connection: V2ConnectionTo<Client>| {
                let services = prompt.clone();
                connection.clone().spawn(async move {
                    run_prompt(&services, request, responder, &connection).await
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::SetSessionConfigOptionRequest,
                        responder: Responder<v2::SetSessionConfigOptionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond_with_result(set_config_option(&config.store, &request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v2::CancelSessionNotification,
                        _connection: V2ConnectionTo<Client>| {
                match cancel
                    .store
                    .cancel_active_turn(&notification.session_id.to_string())
                {
                    Ok(()) | Err(AdapterError::InvalidParams(_)) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// The store lookup's only `InvalidParams` case is an absent session; v2 uses
/// resource-not-found for that case, without changing v1's error contract.
fn require_active_session(store: &SessionStore, id: &v2::SessionId) -> AcpResult<()> {
    store
        .lookup_session(&id.to_string())
        .map_err(|error| match error {
            AdapterError::InvalidParams(_) => {
                agent_client_protocol::Error::resource_not_found(None)
            }
            other => other.into(),
        })
}

fn close_session(
    store: &SessionStore,
    request: &v2::CloseSessionRequest,
) -> AcpResult<v2::CloseSessionResponse> {
    require_active_session(store, &request.session_id)?;
    super::handle_close_session_request(
        store,
        &v1::CloseSessionRequest::new(request.session_id.to_string()),
    )?;
    Ok(v2::CloseSessionResponse::new())
}

/// The v2 handshake. The router only selects this implementation for clients
/// that accept v2, so the answer is always v2. Advertise the session baseline,
/// stdio/HTTP MCP and additional roots, but no prompt or selected-content extensions.
pub(crate) fn initialize_response() -> v2::InitializeResponse {
    v2::InitializeResponse::new(
        ProtocolVersion::V2,
        v2::Implementation::new(ADAPTER_NAME, ADAPTER_VERSION),
    )
    .capabilities(
        v2::AgentCapabilities::new().session(
            v2::SessionCapabilities::new()
                .mcp(
                    v2::McpCapabilities::new()
                        .stdio(v2::McpStdioCapabilities::new())
                        .http(v2::McpHttpCapabilities::new()),
                )
                .additional_directories(v2::SessionAdditionalDirectoriesCapabilities::new()),
        ),
    )
}

/// Validate the complete setup before starting any MCP process or connection.
fn session_setup(
    cwd: &v2::AbsolutePath,
    directories: &[v2::AbsolutePath],
    servers: &[v2::McpServer],
) -> AcpResult<v1::NewSessionRequest> {
    let servers = servers
        .iter()
        .map(|server| {
            Ok(match server {
                v2::McpServer::Stdio(server) => v1::McpServer::Stdio(
                    v1::McpServerStdio::new(&server.name, &server.command.0)
                        .args(server.args.clone())
                        .env(
                            server
                                .env
                                .iter()
                                .map(|env| v1::EnvVariable::new(&env.name, &env.value))
                                .collect(),
                        ),
                ),
                v2::McpServer::Http(server) => v1::McpServer::Http(
                    v1::McpServerHttp::new(&server.name, &server.url).headers(
                        server
                            .headers
                            .iter()
                            .map(|header| v1::HttpHeader::new(&header.name, &header.value))
                            .collect(),
                    ),
                ),
                _ => {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("unsupported MCP transport"));
                }
            })
        })
        .collect::<AcpResult<Vec<_>>>()?;
    let setup = v1::NewSessionRequest::new(&cwd.0)
        .additional_directories(directories.iter().map(|path| path.0.clone()).collect())
        .mcp_servers(servers);
    super::validate_session_paths(&setup)?;
    Ok(setup)
}

async fn new_session(
    store: &SessionStore,
    request: &v2::NewSessionRequest,
) -> AcpResult<v2::NewSessionResponse> {
    let mut setup = session_setup(
        &request.cwd,
        &request.additional_directories,
        &request.mcp_servers,
    )?;
    let sessions = crate::connect_mcp_sessions(&setup.mcp_servers).await?;
    // MCP credentials are connection inputs, not durable session metadata.
    setup.mcp_servers.clear();
    let created = super::insert_session_record(store, &setup, sessions)?;
    let session_id = created.session_id.to_string();
    Ok(v2::NewSessionResponse::new(session_id.clone())
        .config_options(config_options(store, &session_id)?)
        .available_commands(available_commands()))
}

fn list_sessions(
    store: &SessionStore,
    request: &v2::ListSessionsRequest,
) -> AcpResult<v2::ListSessionsResponse> {
    let mut v1_request = v1::ListSessionsRequest::new();
    v1_request.cwd = request.cwd.as_ref().map(|cwd| cwd.0.clone());
    let listed = super::handle_list_sessions_request(store, &v1_request)?;
    Ok(v2::ListSessionsResponse::new(
        listed
            .sessions
            .into_iter()
            .map(|session| {
                v2::SessionInfo::new(session.session_id.to_string(), session.cwd)
                    .additional_directories(
                        session
                            .additional_directories
                            .into_iter()
                            .map(v2::AbsolutePath::new)
                            .collect::<Vec<_>>(),
                    )
                    .title(session.title)
                    .updated_at(session.updated_at)
            })
            .collect(),
    ))
}

/// v2 merges v1's load and resume: without `replayFrom` the session is
/// restored silently, with `replayFrom: start` its history is replayed first.
async fn resume_session(
    store: &SessionStore,
    request: &v2::ResumeSessionRequest,
    connection: &V2ConnectionTo<Client>,
) -> AcpResult<v2::ResumeSessionResponse> {
    let setup = session_setup(
        &request.cwd,
        &request.additional_directories,
        &request.mcp_servers,
    )?;
    if !matches!(&request.replay_from, None | Some(v2::ReplayFrom::Start(_))) {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("only replayFrom start is supported"));
    }
    let (session_id, history) = super::restore_persisted_session(
        store,
        &request.session_id.to_string().into(),
        &setup.cwd,
        Some(&setup),
    )
    .await?;
    if request.replay_from.is_some() {
        super::replay_session_history(&session_id, &history, &mut |notification| {
            match replayed_update(notification.update) {
                Some(update) => send(connection, &request.session_id, update),
                None => Ok(()),
            }
        })?;
    }
    send(connection, &request.session_id, idle(None))?;
    Ok(v2::ResumeSessionResponse::new()
        .config_options(config_options(store, &request.session_id.to_string())?)
        .available_commands(available_commands()))
}

/// Translate v1's history replay into v2 whole-message snapshots.
fn replayed_update(update: v1::SessionUpdate) -> Option<v2::SessionUpdate> {
    match update {
        v1::SessionUpdate::UserMessageChunk(chunk) => {
            let id = chunk.message_id?.to_string();
            Some(v2::SessionUpdate::UserMessage(
                v2::UserMessage::new(id).content(vec![v1_text(&chunk.content).into()]),
            ))
        }
        v1::SessionUpdate::AgentMessageChunk(chunk) => {
            let id = chunk.message_id?.to_string();
            Some(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(id).content(vec![v1_text(&chunk.content).into()]),
            ))
        }
        v1::SessionUpdate::ToolCall(call) => {
            let mut update = v2::ToolCallUpdate::new(call.tool_call_id.to_string())
                .title(call.title)
                .kind(v2_tool_kind(call.kind.into()))
                .status(v2::ToolCallStatus::Completed);
            if let Some(output) = call.raw_output {
                update = update.raw_output(output);
            }
            Some(v2::SessionUpdate::ToolCallUpdate(update))
        }
        _ => None,
    }
}

fn v1_text(content: &v1::ContentBlock) -> String {
    match content {
        v1::ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

/// Turn v2 prompt content into the turn loop's input. The baseline requires
/// text and resource links; links reach the model as Markdown links and are
/// never fetched. Other blocks are not advertised and are refused.
fn prompt_input(request: &v2::PromptRequest) -> AcpResult<PromptInput> {
    let mut parts = Vec::with_capacity(request.prompt.len());
    let mut title = None;
    for block in &request.prompt {
        match block {
            v2::ContentBlock::Text(text) => {
                if !text.text.trim().is_empty() {
                    title = Some(text.text.clone());
                }
                parts.push(text.text.clone());
            }
            v2::ContentBlock::ResourceLink(link) => {
                parts.push(format!("[{}]({})", link.name, link.uri));
            }
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("only text and resource link prompt content is supported"));
            }
        }
    }
    Ok(PromptInput {
        session_id: request.session_id.to_string(),
        text: parts.join("\n"),
        title,
    })
}

/// Run one prompt with v2's split lifecycle: answer `session/prompt` at prompt
/// admission, then report output and completion as session updates.
async fn run_prompt(
    services: &Services,
    request: v2::PromptRequest,
    responder: Responder<v2::PromptResponse>,
    connection: &V2ConnectionTo<Client>,
) -> AcpResult<()> {
    let session_id = request.session_id.clone();
    if let Err(error) = require_active_session(&services.store, &session_id) {
        return responder.respond_with_error(error);
    }
    if is_clear_command(&request) {
        return clear_history(services, &session_id, responder, connection).await;
    }
    let input = match prompt_input(&request) {
        Ok(input) => input,
        Err(error) => return responder.respond_with_error(error),
    };
    let user_text = input.text.clone();
    let mut responder = Some(responder);
    let tools = V2Tools {
        connection: connection.clone(),
    };
    let executor = EditorTools(Some(&tools));
    let result = crate::turn::handle_prompt_request(
        &services.store,
        services.llm_client.as_ref(),
        services.tool_registry.as_ref(),
        Some(&executor),
        input,
        services.max_turn_requests,
        |event| {
            if let TurnEvent::Admitted { history_index } = event {
                let message_id = super::derived_message_id(
                    &session_id.to_string(),
                    history_index,
                    MessageRole::User,
                    &user_text,
                );
                if let Some(responder) = responder.take() {
                    responder.respond(v2::PromptResponse::new(message_id.clone()))?;
                }
                send(
                    connection,
                    &session_id,
                    v2::SessionUpdate::UserMessage(
                        v2::UserMessage::new(message_id).content(request.prompt.clone()),
                    ),
                )?;
                return Ok(send(connection, &session_id, running())?);
            }
            match turn_update(&services.store, &session_id.to_string(), event)? {
                Some(update) => Ok(send(connection, &session_id, update)?),
                None => Ok(()),
            }
        },
    )
    .await;
    match (result, responder) {
        // Never admitted: the prompt was not inserted, so the request fails.
        (Err(error), Some(responder)) => responder.respond_with_error(error.into()),
        (Ok(_), Some(responder)) => responder.respond_with_error(
            agent_client_protocol::Error::internal_error()
                .data("turn completed without prompt admission"),
        ),
        // Admitted: the request already succeeded, so completion is an update.
        (Ok(result), None) => send(connection, &session_id, idle(Some(stop_reason(&result)))),
        (Err(error), None) => {
            // The same sanitized error a v1 prompt response would carry.
            let error = v2_error(&agent_client_protocol::Error::from(error));
            send(
                connection,
                &session_id,
                idle(Some(v2::ErrorStopReason::new().error(error).into())),
            )
        }
    }
}

/// Encode a turn event as a v2 update. Prompt admission is handled by the
/// caller. A mode change reports the session's config options, as the `mode`
/// option carries it in v2.
fn turn_update(
    store: &SessionStore,
    session_id: &str,
    event: TurnEvent,
) -> AcpResult<Option<v2::SessionUpdate>> {
    Ok(Some(match event {
        TurnEvent::Admitted { .. } => return Ok(None),
        TurnEvent::ModeChanged(_) => v2::SessionUpdate::ConfigOptionUpdate(
            v2::ConfigOptionUpdate::new(config_options(store, session_id)?),
        ),
        TurnEvent::SessionInfo { title, updated_at } => {
            let mut info = v2::SessionInfoUpdate::new().updated_at(updated_at);
            if let Some(title) = title {
                info = info.title(title);
            }
            v2::SessionUpdate::SessionInfoUpdate(info)
        }
        TurnEvent::Message {
            text,
            message_id,
            thought,
        } => {
            let chunk = v2::ContentChunk::new(text.into(), message_id);
            if thought {
                v2::SessionUpdate::AgentThoughtChunk(chunk)
            } else {
                v2::SessionUpdate::AgentMessageChunk(chunk)
            }
        }
        TurnEvent::ToolCall { call, kind } => v2::SessionUpdate::ToolCallUpdate(
            v2::ToolCallUpdate::new(call.id().to_string())
                .name(call.name().to_string())
                .title(crate::turn::tool_call_title(&call))
                .kind(v2_tool_kind(kind))
                .status(v2::ToolCallStatus::Pending)
                .raw_input(crate::turn::tool_raw_input(&call)),
        ),
        TurnEvent::ToolResult { call, result } => v2::SessionUpdate::ToolCallUpdate(
            v2::ToolCallUpdate::new(call.id().to_string())
                .status(if result.success {
                    v2::ToolCallStatus::Completed
                } else {
                    v2::ToolCallStatus::Failed
                })
                .content(vec![match &result.edit {
                    Some(edit) => {
                        diff::content(&edit.path, edit.old_text.as_deref(), &edit.new_text)
                    }
                    None => {
                        v2::ToolCallContent::Content(Box::new(v2::Content::new(result.content)))
                    }
                }])
                .raw_output(result.raw_output),
        ),
        TurnEvent::Usage {
            used,
            size,
            cost_micros,
        } => {
            let mut update = v2::UsageUpdate::new(used, size);
            if let Some(micros) = cost_micros {
                let amount = micros
                    .to_string()
                    .parse::<f64>()
                    .map_or(0.0, |value| value / 1_000_000.0);
                update = update.cost(v2::Cost::new(amount, "USD"));
            }
            v2::SessionUpdate::UsageUpdate(update)
        }
    }))
}

/// `/clear` alone clears the conversation, as in v1: no provider request and
/// no inserted message, so the acceptance carries a fresh message ID.
fn is_clear_command(request: &v2::PromptRequest) -> bool {
    matches!(request.prompt.as_slice(), [v2::ContentBlock::Text(text)] if text.text.trim() == "/clear")
}

async fn clear_history(
    services: &Services,
    session_id: &v2::SessionId,
    responder: Responder<v2::PromptResponse>,
    connection: &V2ConnectionTo<Client>,
) -> AcpResult<()> {
    if let Err(error) = services.store.clear_history(&session_id.to_string()).await {
        return responder.respond_with_error(error.into());
    }
    responder.respond(v2::PromptResponse::new(uuid::Uuid::new_v4().to_string()))?;
    send(connection, session_id, running())?;
    send(
        connection,
        session_id,
        v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "Conversation history cleared.".into(),
            uuid::Uuid::new_v4().to_string(),
        )),
    )?;
    send(connection, session_id, idle(Some(v2::StopReason::EndTurn)))
}

/// The session's config options in v2 shape. v1 and v2 share the option
/// structure except that v2 names the identifier `configId`; deserializing
/// checks every other field, so a divergence fails loudly here.
fn config_options(
    store: &SessionStore,
    session_id: &str,
) -> AcpResult<Vec<v2::SessionConfigOption>> {
    store
        .session_config_options(session_id)?
        .into_iter()
        .map(|option| {
            let mut json = serde_json::to_value(option)
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            if let Some(object) = json.as_object_mut()
                && let Some(id) = object.remove("id")
            {
                object.insert("configId".into(), id);
            }
            serde_json::from_value(json).map_err(agent_client_protocol::Error::into_internal_error)
        })
        .collect()
}

fn set_config_option(
    store: &SessionStore,
    request: &v2::SetSessionConfigOptionRequest,
) -> AcpResult<v2::SetSessionConfigOptionResponse> {
    require_active_session(store, &request.session_id)?;
    let value = match &request.value {
        v2::SessionConfigOptionValue::Id { value } => v1::SessionConfigOptionValue::ValueId {
            value: value.to_string().into(),
        },
        v2::SessionConfigOptionValue::Boolean { value } => {
            v1::SessionConfigOptionValue::Boolean { value: *value }
        }
        _ => {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("unsupported session config option value"));
        }
    };
    let session_id = request.session_id.to_string();
    // The response carries the complete option list, so the v1 handler's
    // follow-up notifications have nothing to add.
    super::handle_set_session_config_option_request_notifying(
        store,
        &v1::SetSessionConfigOptionRequest::new(
            session_id.clone(),
            request.config_id.to_string(),
            value,
        ),
        |_| Ok(()),
    )?;
    Ok(v2::SetSessionConfigOptionResponse::new(config_options(
        store,
        &session_id,
    )?))
}

fn stop_reason(result: &PromptResult) -> v2::StopReason {
    use crate::turn::StopReason as Domain;
    match result.stop_reason {
        Domain::EndTurn => v2::StopReason::EndTurn,
        Domain::MaxTokens => v2::StopReason::MaxTokens,
        Domain::MaxTurnRequests => v2::StopReason::MaxTurnRequests,
        Domain::Refusal => v2::StopReason::Refusal,
        Domain::Cancelled => v2::StopReason::Cancelled,
    }
}

fn v2_tool_kind(kind: crate::tools::ToolKind) -> v2::ToolKind {
    use crate::tools::ToolKind as Domain;
    match kind {
        Domain::Read => v2::ToolKind::Read,
        Domain::Search => v2::ToolKind::Search,
        Domain::Edit => v2::ToolKind::Edit,
        Domain::Execute => v2::ToolKind::Execute,
        Domain::Think => v2::ToolKind::Think,
        Domain::Fetch => v2::ToolKind::Fetch,
        Domain::Other => v2::ToolKind::Other,
    }
}

fn available_commands() -> Vec<v2::AvailableCommand> {
    adapter_available_commands()
        .into_iter()
        .map(|command| {
            let mut available = v2::AvailableCommand::new(command.name, command.description);
            if let Some(v1::AvailableCommandInput::Unstructured(input)) = command.input {
                available = available.input(v2::AvailableCommandInput::Text(
                    v2::TextCommandInput::new(input.hint),
                ));
            }
            available
        })
        .collect()
}

fn v2_error(error: &agent_client_protocol::Error) -> v2::Error {
    v2::Error::new(i32::from(error.code), error.message.clone()).data(error.data.clone())
}

fn running() -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(v2::RunningStateUpdate::new()))
}

fn idle(stop_reason: Option<v2::StopReason>) -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
        v2::IdleStateUpdate::new().stop_reason(stop_reason),
    ))
}

fn send(
    connection: &V2ConnectionTo<Client>,
    session_id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> AcpResult<()> {
    connection.send_notification(v2::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "Test assertions index deliberately; a panic is a failure."
)]
mod tests {
    use super::*;
    use crate::turn::StopReason as Domain;

    fn prompt(blocks: Vec<v2::ContentBlock>) -> v2::PromptRequest {
        v2::PromptRequest::new("session-1", blocks)
    }

    #[test]
    fn prompt_input_joins_text_and_resource_links_and_titles_by_text() -> AcpResult<()> {
        let input = prompt_input(&prompt(vec![
            "explain".into(),
            v2::ContentBlock::ResourceLink(v2::ResourceLink::new("lib.rs", "file:///src/lib.rs")),
        ]))?;
        assert_eq!(input.session_id, "session-1");
        assert_eq!(input.text, "explain\n[lib.rs](file:///src/lib.rs)");
        assert_eq!(input.title.as_deref(), Some("explain"));
        Ok(())
    }

    #[test]
    fn prompt_input_refuses_unadvertised_content() {
        let image = v2::ContentBlock::Image(v2::ImageContent::new("AAAA", "image/png"));
        assert!(prompt_input(&prompt(vec![image])).is_err());
    }

    #[test]
    fn turn_events_encode_as_v2_updates() -> Result<(), Box<dyn std::error::Error>> {
        let store = crate::test_store();
        let encode = |event| turn_update(&store, "unused", event);
        let message = |thought| TurnEvent::Message {
            text: "hi".into(),
            message_id: "m-1".into(),
            thought,
        };
        assert!(matches!(
            encode(message(false))?,
            Some(v2::SessionUpdate::AgentMessageChunk(chunk)) if chunk.message_id.to_string() == "m-1"
        ));
        assert!(matches!(
            encode(message(true))?,
            Some(v2::SessionUpdate::AgentThoughtChunk(_))
        ));
        assert!(encode(TurnEvent::Admitted { history_index: 0 })?.is_none());
        let Some(v2::SessionUpdate::UsageUpdate(usage)) = encode(TurnEvent::Usage {
            used: 10,
            size: 100,
            cost_micros: Some(1_500_000),
        })?
        else {
            return Err("usage was not encoded".into());
        };
        assert_eq!((usage.used, usage.size), (10, 100));
        assert_eq!(usage.cost.map(|cost| cost.amount), Some(1.5));
        Ok(())
    }

    #[test]
    fn stop_reasons_map_one_to_one() {
        for (domain, expected) in [
            (Domain::EndTurn, v2::StopReason::EndTurn),
            (Domain::MaxTokens, v2::StopReason::MaxTokens),
            (Domain::MaxTurnRequests, v2::StopReason::MaxTurnRequests),
            (Domain::Refusal, v2::StopReason::Refusal),
            (Domain::Cancelled, v2::StopReason::Cancelled),
        ] {
            let result = PromptResult {
                stop_reason: domain,
                usage: None,
            };
            assert_eq!(stop_reason(&result), expected);
        }
    }

    #[test]
    fn replay_keeps_message_ids_and_drops_unknown_updates() {
        let user = v1::SessionUpdate::UserMessageChunk(
            v1::ContentChunk::new("hello".into()).message_id(v1::MessageId::new("u-1")),
        );
        assert!(matches!(
            replayed_update(user),
            Some(v2::SessionUpdate::UserMessage(message)) if message.message_id.to_string() == "u-1"
        ));
        let without_id = v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new("x".into()));
        assert!(replayed_update(without_id).is_none());
    }
}
