//! Draft ACP v2 agent for the `protocol-v2` probe (daa-acp-v2-37zc).
//!
//! `serve` routes each connection by its `initialize` request: v1 clients reach
//! the stable implementation in the parent module, v2 clients reach this one.
//! The v2 wire surface is translated here, at the edge, onto the same session
//! store, v1 session handlers and turn loop, so validation, persistence and
//! confinement are not duplicated.
//!
//! The probe implements the v2 session baseline (new, list, resume, close,
//! prompt, cancel and updates). Tool execution and config options are added by
//! daa-acp-v2-37zc.4; until then this module runs turns without a tool
//! executor.

use std::num::NonZeroUsize;
use std::sync::Arc;

use acp_llm_adapter::llm::{LlmClient, MessageRole};
use agent_client_protocol::schema::v1;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, V2ConnectionTo};

use crate::session_store::SessionStore;
use crate::tools::ToolRegistry;
use crate::turn::{PromptInput, PromptResult, TurnEvent};
use crate::{ADAPTER_NAME, ADAPTER_VERSION, adapter_available_commands};

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
    let (new, list, resume, close, prompt, cancel) = (
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
                match new_session(&new.store, &request) {
                    Ok(response) => {
                        let session_id = response.session_id.clone();
                        responder.respond(response)?;
                        // A new session is ready for its first prompt.
                        send(&connection, &session_id, idle(None))
                    }
                    Err(error) => responder.respond_with_error(error),
                }
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
                let closed = super::handle_close_session_request(
                    &close.store,
                    &v1::CloseSessionRequest::new(request.session_id.to_string()),
                )
                .map(|_| v2::CloseSessionResponse::new());
                responder.respond_with_result(closed)
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
        .on_receive_notification(
            async move |notification: v2::CancelSessionNotification,
                        _connection: V2ConnectionTo<Client>| {
                cancel
                    .store
                    .cancel_active_turn(&notification.session_id.to_string())?;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// The v2 handshake. The router only selects this implementation for clients
/// that accept v2, so the answer is always v2. `session: {}` advertises the
/// whole baseline session surface and nothing optional: no MCP, no prompt
/// extensions and no selected-content extension.
pub(crate) fn initialize_response() -> v2::InitializeResponse {
    v2::InitializeResponse::new(
        ProtocolVersion::V2,
        v2::Implementation::new(ADAPTER_NAME, ADAPTER_VERSION),
    )
    .capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()))
}

/// Refuse lifecycle fields for capabilities the probe does not advertise.
fn reject_unadvertised(
    mcp_servers: &[v2::McpServer],
    additional_directories: &[v2::AbsolutePath],
) -> AcpResult<()> {
    if !mcp_servers.is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("MCP servers are not supported by this ACP v2 agent"));
    }
    if !additional_directories.is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("additionalDirectories is not advertised by this ACP v2 agent"));
    }
    Ok(())
}

fn new_session(
    store: &SessionStore,
    request: &v2::NewSessionRequest,
) -> AcpResult<v2::NewSessionResponse> {
    reject_unadvertised(&request.mcp_servers, &request.additional_directories)?;
    let created =
        super::handle_new_session_request(store, &v1::NewSessionRequest::new(&request.cwd.0))?;
    Ok(v2::NewSessionResponse::new(created.session_id.to_string())
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
    reject_unadvertised(&request.mcp_servers, &request.additional_directories)?;
    let session_id = request.session_id.to_string();
    match &request.replay_from {
        None => {
            super::handle_resume_session_request(
                store,
                &v1::ResumeSessionRequest::new(session_id, &request.cwd.0),
            )
            .await?;
        }
        Some(v2::ReplayFrom::Start(_)) => {
            super::handle_load_session_request(
                store,
                &v1::LoadSessionRequest::new(session_id, &request.cwd.0),
                |notification| match replayed_update(notification.update) {
                    Some(update) => send(connection, &request.session_id, update),
                    None => Ok(()),
                },
            )
            .await?;
        }
        Some(_) => {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("only replayFrom start is supported"));
        }
    }
    Ok(v2::ResumeSessionResponse::new().available_commands(available_commands()))
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
    let input = match prompt_input(&request) {
        Ok(input) => input,
        Err(error) => return responder.respond_with_error(error),
    };
    let user_text = input.text.clone();
    let mut responder = Some(responder);
    let result = crate::turn::handle_prompt_request(
        &services.store,
        services.llm_client.as_ref(),
        services.tool_registry.as_ref(),
        None,
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
            match turn_update(event) {
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
/// caller; mode changes arrive with config options (daa-acp-v2-37zc.4).
fn turn_update(event: TurnEvent) -> Option<v2::SessionUpdate> {
    Some(match event {
        TurnEvent::Admitted { .. } | TurnEvent::ModeChanged(_) => return None,
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
    })
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
    fn turn_events_encode_as_v2_updates() -> Result<(), String> {
        let message = |thought| TurnEvent::Message {
            text: "hi".into(),
            message_id: "m-1".into(),
            thought,
        };
        assert!(matches!(
            turn_update(message(false)),
            Some(v2::SessionUpdate::AgentMessageChunk(chunk)) if chunk.message_id.to_string() == "m-1"
        ));
        assert!(matches!(
            turn_update(message(true)),
            Some(v2::SessionUpdate::AgentThoughtChunk(_))
        ));
        assert!(turn_update(TurnEvent::Admitted { history_index: 0 }).is_none());
        let Some(v2::SessionUpdate::UsageUpdate(usage)) = turn_update(TurnEvent::Usage {
            used: 10,
            size: 100,
            cost_micros: Some(1_500_000),
        }) else {
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
