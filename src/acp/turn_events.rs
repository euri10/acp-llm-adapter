//! Encode domain turn events as ACP notifications at the transport edge.

use crate::SessionStore;
use crate::tools::ToolExecution;
use crate::turn::tool_call_title;
use crate::turn::{PromptResult, TurnEvent, UsageTotals};
use crate::{session_notification, tool_raw_input};
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::ToolCall as ChatToolCall;
use agent_client_protocol::schema::v1::ToolCall as AcpToolCall;
use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, ContentChunk, Cost, CurrentModeUpdate, Diff, MessageId, Plan,
    PromptResponse, SessionId, SessionInfoUpdate, SessionNotification, SessionUpdate, StopReason,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, Usage, UsageUpdate,
};

pub(crate) fn encode_event(
    store: &SessionStore,
    session_id: &SessionId,
    event: TurnEvent,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), AdapterError> {
    let update = match event {
        TurnEvent::SessionInfo { title, updated_at } => {
            let mut info = SessionInfoUpdate::new().updated_at(updated_at);
            if let Some(title) = title {
                info = info.title(title);
            }
            SessionUpdate::SessionInfoUpdate(info)
        }
        TurnEvent::Message {
            text,
            message_id,
            thought,
        } => {
            let chunk = ContentChunk::new(text.into()).message_id(MessageId::new(message_id));
            if thought {
                SessionUpdate::AgentThoughtChunk(chunk)
            } else {
                SessionUpdate::AgentMessageChunk(chunk)
            }
        }
        TurnEvent::ToolCall { call, kind } => {
            return report_tool_call(session_id, notify, &call, kind.into());
        }
        TurnEvent::ToolResult { call, result } => {
            return report_tool_result(session_id, notify, &call, &result);
        }
        TurnEvent::ModeChanged(mode) => {
            notify(session_notification(
                session_id.clone(),
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode.mode_id())),
            ))?;
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                store.session_config_options(&session_id.0)?,
            ))
        }
        TurnEvent::Usage {
            used,
            size,
            cost_micros,
        } => {
            let mut update = UsageUpdate::new(used, size);
            if let Some(micros) = cost_micros {
                let amount = micros
                    .to_string()
                    .parse::<f64>()
                    .map_or(0.0, |value| value / 1_000_000.0);
                update = update.cost(Cost::new(amount, "USD"));
            }
            SessionUpdate::UsageUpdate(update)
        }
    };
    notify(session_notification(session_id.clone(), update))?;
    Ok(())
}

pub(crate) fn encode_usage(usage: UsageTotals) -> Usage {
    Usage::new(usage.total_tokens, usage.input_tokens, usage.output_tokens)
        .thought_tokens(usage.thought_tokens)
        .cached_read_tokens(usage.cached_read_tokens)
        .cached_write_tokens(usage.cached_write_tokens)
}

pub(crate) fn encode_result(result: PromptResult) -> PromptResponse {
    use crate::turn::StopReason as Domain;
    let reason = match result.stop_reason {
        Domain::EndTurn => StopReason::EndTurn,
        Domain::MaxTokens => StopReason::MaxTokens,
        Domain::MaxTurnRequests => StopReason::MaxTurnRequests,
        Domain::Refusal => StopReason::Refusal,
        Domain::Cancelled => StopReason::Cancelled,
    };
    PromptResponse::new(reason).usage(result.usage.map(encode_usage))
}

pub(crate) fn report_tool_call(
    session_id: &SessionId,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
    call: &ChatToolCall,
    kind: ToolKind,
) -> Result<(), AdapterError> {
    let title = tool_call_title(call);
    notify(session_notification(
        session_id.clone(),
        SessionUpdate::ToolCall(
            AcpToolCall::new(call.id().to_string(), title)
                .kind(kind)
                .status(ToolCallStatus::Pending)
                .raw_input(tool_raw_input(call)),
        ),
    ))?;
    Ok(())
}

pub(crate) fn report_tool_result(
    session_id: &SessionId,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
    call: &ChatToolCall,
    result: &ToolExecution,
) -> Result<(), AdapterError> {
    let mut fields = ToolCallUpdateFields::new()
        .status(if result.success {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        })
        .content(tool_call_update_content(result))
        .raw_output(result.raw_output.clone());

    if let Some(edit) = &result.edit {
        fields = fields.locations(vec![
            ToolCallLocation::new(edit.path.clone()).line(edit.line),
        ]);
    }

    notify(session_notification(
        session_id.clone(),
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(call.id().to_string(), fields)),
    ))?;

    if result.success && call.name() == "update_plan" {
        let plan = serde_json::from_value::<Plan>(result.raw_output.clone()).map_err(|error| {
            AdapterError::Internal(format!("invalid update_plan result: {error}"))
        })?;
        notify(session_notification(
            session_id.clone(),
            SessionUpdate::Plan(plan),
        ))?;
    }
    Ok(())
}

fn tool_call_update_content(result: &ToolExecution) -> Vec<ToolCallContent> {
    match &result.edit {
        Some(edit) => vec![ToolCallContent::from(
            Diff::new(edit.path.clone(), edit.new_text.clone()).old_text(edit.old_text.clone()),
        )],
        None => vec![ToolCallContent::from(result.content.clone())],
    }
}

impl From<crate::tools::ToolKind> for ToolKind {
    fn from(kind: crate::tools::ToolKind) -> Self {
        use crate::tools::ToolKind as Domain;
        match kind {
            Domain::Read => Self::Read,
            Domain::Search => Self::Search,
            Domain::Edit => Self::Edit,
            Domain::Execute => Self::Execute,
            Domain::Think => Self::Think,
            Domain::Fetch => Self::Fetch,
            Domain::Other => Self::Other,
        }
    }
}
impl From<ToolKind> for crate::tools::ToolKind {
    fn from(kind: ToolKind) -> Self {
        match kind {
            ToolKind::Read => Self::Read,
            ToolKind::Search => Self::Search,
            ToolKind::Edit => Self::Edit,
            ToolKind::Execute => Self::Execute,
            ToolKind::Think => Self::Think,
            ToolKind::Fetch => Self::Fetch,
            _ => Self::Other,
        }
    }
}
