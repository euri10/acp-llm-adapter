//! Prompt-turn orchestration isolated from ACP transport wiring.

use std::num::NonZeroUsize;

use acp_llm_adapter::llm::{
    ChatMessage, ChatRequest, FinishReason, LlmClient, MessageRole, StreamEvent,
    ToolCall as ChatToolCall, ToolDefinition, UsageData,
};
use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, ContentChunk, Diff, MessageId, Plan, PromptRequest, PromptResponse,
    SessionId, SessionInfoUpdate, SessionNotification, SessionUpdate, StopReason,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, Usage, UsageUpdate,
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::acp::ToolCallRequester;
use crate::tools::{ToolContext, ToolExecution, ToolRegistry};
use crate::{
    PendingToolCalls, ReasoningEffort, SessionBehavior, SessionStore, session_notification,
    stop_reason_from_finish, text_from_prompt,
};
use acp_llm_adapter::error::AdapterError;

/// Stable model settings applied to each streamed LLM request in a prompt turn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModelRequestSettings<'a> {
    /// Selected model identifier.
    pub(crate) model: &'a str,
    /// Reasoning effort requested from the model, if explicitly configured.
    /// `None` means use the model's default — omit the parameter from the request.
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Maximum tokens the model may generate. `None` means use the model's
    /// default — omit the parameter from the request.
    pub(crate) max_tokens: Option<u32>,
}

struct PromptTurnEnvironment<'a> {
    store: &'a SessionStore,
    llm_client: &'a dyn LlmClient,
    tool_registry: &'a dyn ToolRegistry,
    connection: Option<&'a dyn ToolCallRequester>,
    tool_context: ToolContext,
    behavior: SessionBehavior,
    request: PromptRequest,
    cancellation_token: CancellationToken,
    max_turn_requests: NonZeroUsize,
}

/// Filter messages to fit within a byte budget, keeping the first and most recent messages.
///
/// The provider API enforces a request size limit (e.g. ~1MB for CloudFront-backed endpoints).
/// We filter messages to stay well under this limit (512KB budget) to ensure requests
/// complete successfully. The filter keeps the first message and as many recent
/// tool-call units as fit within the budget, dropping older messages if needed. A
/// single oversized recent message never blocks smaller, older messages from
/// also being considered.
///
/// # Arguments
///
/// * `messages` - All messages in the conversation
/// * `max_bytes` - Maximum bytes allowed for the filtered message list
///
/// # Returns
///
/// A filtered message list that fits within the byte budget.
fn filter_messages_by_size(messages: &[ChatMessage], max_bytes: usize) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Calculate total size - if it fits, return as-is
    let total_size: usize = messages.iter().map(estimate_message_size).sum();
    if total_size <= max_bytes {
        return messages.to_vec();
    }

    filter_messages_truncate(messages, max_bytes)
}

#[allow(clippy::indexing_slicing)]
fn filter_messages_truncate(messages: &[ChatMessage], max_bytes: usize) -> Vec<ChatMessage> {
    // Keep the first message (the oldest message in the session) unconditionally,
    // then fill the remaining budget with the most recent tool-call units that fit.
    let mut filtered = Vec::new();
    let mut budget = max_bytes;

    let first = messages[0].clone();
    let first_size = estimate_message_size(&first);
    filtered.push(first);
    budget = budget.saturating_sub(first_size);

    // Group the rest so an assistant message requesting tool calls always
    // travels with the tool results answering it - truncation must keep or
    // drop such a pair together, never split it.
    let groups = group_tool_call_units(&messages[1..]);

    // Walk groups from most recent to oldest. A single oversized recent group
    // must not stop older, smaller groups from also being considered -
    // otherwise one large tool result collapses the whole history down to
    // just the pinned first message.
    let mut kept_groups = Vec::new();
    for group in groups.iter().rev() {
        let group_size: usize = group.iter().map(estimate_message_size).sum();
        if group_size > budget {
            continue;
        }
        budget = budget.saturating_sub(group_size);
        kept_groups.push(group);
    }
    kept_groups.reverse();
    for group in kept_groups {
        filtered.extend_from_slice(group);
    }

    // Validate tool result messages: only keep tool results if the corresponding
    // tool call is present in the filtered messages. This prevents orphaned tool
    // results from causing 400 Bad Request errors from the LLM API.
    validate_tool_results(&filtered)
}

/// Group messages so an assistant message requesting tool calls stays with
/// the tool result messages that answer it.
///
/// Truncation operates on these groups as atomic units so it can never keep
/// an assistant tool call without its result (or vice versa).
fn group_tool_call_units(messages: &[ChatMessage]) -> Vec<Vec<ChatMessage>> {
    let mut groups = Vec::new();
    let mut iter = messages.iter().peekable();
    while let Some(msg) = iter.next() {
        let mut group = vec![msg.clone()];
        if msg.role() == MessageRole::Assistant && !msg.tool_calls().is_empty() {
            while let Some(next) = iter.peek() {
                if next.role() == MessageRole::Tool {
                    group.push((*next).clone());
                    iter.next();
                } else {
                    break;
                }
            }
        }
        groups.push(group);
    }
    groups
}

/// Ensure tool result messages have corresponding tool calls in the message history.
///
/// Removes any tool result messages whose referenced `tool_call_id` doesn't appear
/// in an assistant message within the filtered history. This prevents orphaned
/// tool responses which violate the OpenAI-compatible API contract.
#[allow(clippy::indexing_slicing)]
fn validate_tool_results(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    // Collect all tool call IDs from assistant messages
    let mut available_tool_calls = std::collections::HashSet::new();
    for msg in messages {
        if msg.role() == MessageRole::Assistant {
            for tool_call in msg.tool_calls() {
                available_tool_calls.insert(tool_call.id().to_string());
            }
        }
    }

    // Filter out tool result messages with missing tool calls
    messages
        .iter()
        .filter(|msg| {
            if msg.role() == MessageRole::Tool {
                // Keep tool result only if the tool call exists
                if let Some(tool_call_id) = msg.tool_call_id() {
                    available_tool_calls.contains(tool_call_id)
                } else {
                    // Tool result without ID - invalid, drop it
                    false
                }
            } else {
                // Keep all non-tool messages
                true
            }
        })
        .cloned()
        .collect()
}

/// Repair a filtered message list into a shape the provider's chat API accepts.
///
/// Size-based filtering ([`filter_messages_by_size`]) drops assistant+tool
/// groups from the middle of the conversation, leaving two artifacts that
/// the provider rejects with 400 Bad Request:
///
/// - **Empty assistant messages** (no content and no tool calls), which
///   serialize to `{"role":"assistant"}` and violate the API contract. These
///   can also be carried in unfiltered from history.
/// - **Consecutive same-role user/assistant messages**, produced when the
///   groups that separated them were dropped, breaking role alternation.
///
/// This pass drops empty assistant messages and coalesces adjacent same-role
/// user/assistant text messages, joining their content. Tool messages and
/// assistant messages that carry tool calls are never merged or dropped, so the
/// tool-call/result pairing established by [`filter_messages_truncate`] and
/// [`validate_tool_results`] is preserved.
fn sanitize_conversation(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut sanitized: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        // Drop empty assistant messages: nothing to say and no tool call to make.
        if message.role() == MessageRole::Assistant
            && message.tool_calls().is_empty()
            && message.content().trim().is_empty()
        {
            continue;
        }

        // Coalesce with the previous message when both are plain-text messages of
        // the same role (user, or assistant without tool calls). This repairs the
        // consecutive-role runs that truncation introduces. Tool messages and
        // assistant-with-tool-calls are excluded, so pairing stays intact.
        let mergeable = matches!(message.role(), MessageRole::User | MessageRole::Assistant)
            && message.tool_calls().is_empty();
        if mergeable
            && let Some(previous) = sanitized.last()
            && previous.role() == message.role()
            && previous.tool_calls().is_empty()
        {
            let merged = format!("{}\n\n{}", previous.content(), message.content());
            let rebuilt = match message.role() {
                MessageRole::User => ChatMessage::user(merged),
                _ => ChatMessage::assistant(merged),
            };
            sanitized.pop();
            sanitized.push(rebuilt);
            continue;
        }

        sanitized.push(message);
    }
    sanitized
}

fn request_messages_for_behavior(
    behavior: SessionBehavior,
    messages: &[ChatMessage],
) -> Vec<ChatMessage> {
    let mut request_messages = messages.to_vec();
    if behavior == SessionBehavior::Plan {
        request_messages.insert(0, plan_mode_instruction_message());
    }
    request_messages
}

fn plan_mode_instruction_message() -> ChatMessage {
    ChatMessage::system(
        "You are in Plan mode. Do not modify files, run shell commands, or use MCP tools. \
Use read-only tools to inspect the codebase, call update_plan when useful, and return a \
concrete step-by-step implementation plan.",
    )
}

/// Estimate the size of a message in bytes for filtering purposes.
///
/// Accounts for JSON serialization overhead (quotes, escapes, delimiters).
/// Raw content alone underestimates the serialized size.
fn estimate_message_size(msg: &ChatMessage) -> usize {
    // Account for JSON serialization overhead using integer arithmetic:
    // - Role field + delimiters: ~10 bytes
    // - Content as quoted string: (content.len() * 21) / 20 ≈ content.len() * 1.05
    // - Tool calls with IDs and function info: ~150 bytes each
    let base: usize = 10;
    let content_len = msg.content().len();
    let content_overhead = (content_len.saturating_mul(21)) / 20;
    let tool_overhead = msg.tool_calls().len().saturating_mul(150);
    base + content_overhead + tool_overhead
}

/// Run the full prompt-turn lifecycle for a single ACP `session/prompt` request.
///
/// This keeps ACP request translation in [`crate::acp`] while moving model
/// streaming, tool-call execution, cancellation handling, plan streaming, and
/// history updates into a dedicated module.
///
/// # Errors
///
/// Returns an ACP protocol error when the prompt is invalid, session setup
/// fails, a streamed model event fails, a tool notification fails, or the
/// session store cannot be updated.
pub(crate) async fn handle_prompt_request(
    store: &SessionStore,
    llm_client: &dyn LlmClient,
    tool_registry: &dyn ToolRegistry,
    connection: Option<&dyn ToolCallRequester>,
    request: PromptRequest,
    max_turn_requests: NonZeroUsize,
    mut notify: impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
) -> Result<PromptResponse, AdapterError> {
    let user_text = text_from_prompt(&request.prompt)?;
    let user_message = ChatMessage::user(user_text.clone());
    let session_id = request.session_id.clone();
    let cancellation_token = CancellationToken::new();

    let turn_setup = store.begin_turn(
        &request.session_id,
        cancellation_token.clone(),
        user_message,
    )?;

    let result = async {
        notify(session_notification(session_id.clone(), {
            let mut session_info_update =
                SessionInfoUpdate::new().updated_at(turn_setup.updated_at.clone());
            if turn_setup.title_changed {
                session_info_update = session_info_update.title(turn_setup.title.clone());
            }
            SessionUpdate::SessionInfoUpdate(session_info_update)
        }))?;

        // Only send `reasoning_effort` when explicitly configured to a non-default
        // value. Omit it for the default (`High`) — the model uses its own default
        // reasoning effort, and some OpenAI-compatible APIs reject unknown
        // parameters with 400 Bad Request.
        let reasoning_effort = (turn_setup.reasoning_effort != ReasoningEffort::High)
            .then_some(turn_setup.reasoning_effort);

        run_prompt_turn(
            PromptTurnEnvironment {
                store,
                llm_client,
                tool_registry,
                connection,
                tool_context: turn_setup.tool_context,
                behavior: turn_setup.behavior,
                request,
                cancellation_token: cancellation_token.clone(),
                max_turn_requests,
            },
            turn_setup.messages,
            ModelRequestSettings {
                model: &turn_setup.model,
                reasoning_effort,
                max_tokens: turn_setup.max_tokens,
            },
            &mut notify,
        )
        .await
    }
    .await;
    let clear_result = match store.clear_active_turn(&session_id) {
        Ok(()) => Ok(()),
        Err(AdapterError::InvalidParams(msg)) if msg.starts_with("unknown session id:") => Ok(()),
        Err(err) => Err(err),
    };
    match (result, clear_result) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), Ok(())) => Err(error),
        (Ok(_response), Err(error)) => Err(error),
        (Err(error), Err(clear_error)) => {
            tracing::warn!(error = ?clear_error, "failed to clear active turn after prompt error");
            Err(error)
        }
    }
}

async fn run_prompt_turn(
    env: PromptTurnEnvironment<'_>,
    mut messages: Vec<ChatMessage>,
    model_settings: ModelRequestSettings<'_>,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
) -> Result<PromptResponse, AdapterError> {
    let tool_definitions = env
        .tool_registry
        .definitions(&env.tool_context, env.store)?
        .into_iter()
        .filter(|definition| {
            env.behavior
                .allows_tool_kind(env.tool_registry.kind(definition.name()))
        })
        .collect::<Vec<_>>();

    let mut stop_reason = StopReason::MaxTurnRequests;
    let mut accumulated_input_tokens: u64 = 0;
    let mut accumulated_output_tokens: u64 = 0;

    for _ in 0..env.max_turn_requests.get() {
        let request_messages = request_messages_for_behavior(env.behavior, &messages);
        let turn = stream_model_turn(
            env.llm_client,
            &request_messages,
            &tool_definitions,
            model_settings,
            env.cancellation_token.clone(),
            &env.request.session_id,
            notify,
        )
        .await?;

        if let Some(ref usage) = turn.usage {
            accumulated_input_tokens += usage.input_tokens;
            accumulated_output_tokens += usage.output_tokens;
        }

        if turn.stop_reason == StopReason::Cancelled {
            stop_reason = StopReason::Cancelled;
            break;
        }

        messages.push(if turn.tool_calls.is_empty() {
            ChatMessage::assistant(turn.assistant_text.clone())
        } else {
            ChatMessage::assistant_with_tool_calls(
                turn.assistant_text.clone(),
                turn.tool_calls.clone(),
            )
        });

        if !matches!(turn.finish_reason, FinishReason::ToolCalls) || turn.tool_calls.is_empty() {
            stop_reason = turn.stop_reason;
            // Persist before exiting — this is the final assistant answer.
            env.store.save_history(&env.request.session_id, &messages)?;
            break;
        }

        let mut pending_mode_transition = None;
        for tool_call in &turn.tool_calls {
            let tool_kind = env.tool_registry.kind(tool_call.name());
            report_tool_call(&env.request.session_id, notify, tool_call, tool_kind)?;
            let tool_result = if env.behavior.allows_tool_kind(tool_kind) {
                env.tool_registry
                    .execute(
                        tool_call,
                        &env.tool_context,
                        env.store,
                        env.connection,
                        env.cancellation_token.clone(),
                    )
                    .await
            } else {
                ToolExecution::failed(format!(
                    "{} mode refuses {} tool calls",
                    env.behavior.mode_id().0.as_ref(),
                    tool_call.name()
                ))
            };
            report_tool_result(&env.request.session_id, notify, tool_call, &tool_result)?;
            if let Some(mode) = transition_mode_from_tool_result(tool_call, &tool_result)? {
                pending_mode_transition = Some(mode);
            }
            messages.push(ChatMessage::tool_result(
                tool_call.id(),
                tool_result.content_for_model(),
            ));
        }

        if let Some(mode) = pending_mode_transition {
            env.store.set_mode(&env.request.session_id, mode)?;
        }

        // Persist after every complete turn cycle (assistant text + tool results).
        // If the process crashes during the next LLM stream, history up to this
        // point is already on disk and can be resumed.
        env.store.save_history(&env.request.session_id, &messages)?;

        if let Some(mode) = pending_mode_transition {
            emit_mode_transition_notifications(env.store, &env.request.session_id, mode, notify)?;
            stop_reason = StopReason::EndTurn;
            break;
        }
    }

    let acp_usage = (accumulated_input_tokens > 0 || accumulated_output_tokens > 0).then(|| {
        Usage::new(
            accumulated_input_tokens + accumulated_output_tokens,
            accumulated_input_tokens,
            accumulated_output_tokens,
        )
    });
    Ok(PromptResponse::new(stop_reason).usage(acp_usage))
}

fn transition_mode_from_tool_result(
    call: &ChatToolCall,
    result: &ToolExecution,
) -> Result<Option<SessionBehavior>, AdapterError> {
    if call.name() != "exit_plan_mode" || !result.success {
        return Ok(None);
    }

    let Some(mode_id) = result
        .raw_output
        .get("mode_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Err(AdapterError::Internal(
            "exit_plan_mode result is missing a mode_id".to_string(),
        ));
    };

    let Some(mode) = SessionBehavior::from_mode_id_str(mode_id) else {
        return Err(AdapterError::Internal(format!(
            "exit_plan_mode returned unsupported mode: {mode_id}"
        )));
    };

    Ok(Some(mode))
}

fn emit_mode_transition_notifications(
    store: &SessionStore,
    session_id: &SessionId,
    mode: SessionBehavior,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    notify(session_notification(
        session_id.clone(),
        SessionUpdate::CurrentModeUpdate(
            agent_client_protocol::schema::v1::CurrentModeUpdate::new(mode.mode_id()),
        ),
    ))?;
    let config_options = store.session_config_options(session_id)?;
    notify(session_notification(
        session_id.clone(),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options)),
    ))?;
    Ok(())
}

/// Stream a single LLM turn, collecting assistant text and pending tool calls.
///
/// # Errors
///
/// Returns an ACP protocol error when the underlying LLM stream fails, when a
/// streamed tool-call delta cannot be assembled into a complete call, or when
/// a session update notification fails.
#[allow(clippy::too_many_lines)]
pub(crate) async fn stream_model_turn(
    llm_client: &dyn LlmClient,
    messages: &[ChatMessage],
    tool_definitions: &[ToolDefinition],
    model_settings: ModelRequestSettings<'_>,
    cancellation_token: CancellationToken,
    session_id: &SessionId,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
) -> Result<ModelTurn, AdapterError> {
    // Filter messages to respect CloudFront's ~1MB request limit.
    // Allocate a conservative 256KB budget for messages to leave ample headroom for:
    // - Tool definitions (can be 100KB+ with long descriptions)
    // - JSON serialization overhead (quotes, escapes, structure)
    // - Request metadata (model, stream flag, etc.)
    let max_message_bytes = 256 * 1024; // 256KB
    let filtered_messages = filter_messages_by_size(messages, max_message_bytes);

    if filtered_messages.len() < messages.len() {
        tracing::warn!(
            total_messages = messages.len(),
            kept_messages = filtered_messages.len(),
            "truncated conversation history to fit request size limit"
        );
    }

    // Repair the structural artifacts that size filtering (and history) can
    // leave behind - consecutive same-role messages and empty assistant
    // messages - which the provider otherwise rejects with 400 Bad Request.
    let filtered_messages = sanitize_conversation(filtered_messages);

    let mut chat_request = ChatRequest::new(filtered_messages)
        .with_tools(tool_definitions.to_vec())
        .with_model(model_settings.model);
    if let Some(effort) = model_settings.reasoning_effort {
        chat_request = chat_request.with_reasoning_effort(effort.id());
    }
    if let Some(max_tokens) = model_settings.max_tokens {
        chat_request = chat_request.with_max_tokens(max_tokens);
    }

    let mut stream = llm_client
        .stream_chat(chat_request, cancellation_token.clone())
        .map_err(AdapterError::from)?;
    let mut assistant_text = String::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut finish_reason = FinishReason::EndTurn;
    let mut tool_calls = PendingToolCalls::default();
    let mut usage: Option<UsageData> = None;
    let mut thought_message_id: Option<MessageId> = None;
    let mut assistant_message_id: Option<MessageId> = None;

    loop {
        let event = tokio::select! {
            () = cancellation_token.cancelled() => {
                stop_reason = StopReason::Cancelled;
                break;
            }
            event = stream.next() => event,
        };

        let Some(event) = event else {
            if cancellation_token.is_cancelled() {
                stop_reason = StopReason::Cancelled;
            }
            break;
        };

        match event.map_err(AdapterError::from)? {
            StreamEvent::Thought(chunk) => {
                let message_id = thought_message_id
                    .get_or_insert_with(|| Uuid::new_v4().to_string().into())
                    .clone();
                notify(session_notification(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(
                        ContentChunk::new(chunk.into()).message_id(message_id),
                    ),
                ))?;
            }
            StreamEvent::Message(chunk) => {
                assistant_text.push_str(&chunk);
                let message_id = assistant_message_id
                    .get_or_insert_with(|| Uuid::new_v4().to_string().into())
                    .clone();
                notify(session_notification(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(
                        ContentChunk::new(chunk.into()).message_id(message_id),
                    ),
                ))?;
            }
            StreamEvent::ToolCallDelta(delta) => tool_calls.push(&delta),
            StreamEvent::Finished(reason) => {
                stop_reason = stop_reason_from_finish(&reason);
                finish_reason = reason;
            }
            StreamEvent::Usage(data) => {
                tracing::debug!(
                    input_tokens = data.input_tokens,
                    output_tokens = data.output_tokens,
                    context_length = data.context_length,
                    "received usage data from stream"
                );
                usage = Some(data);
            }
        }
    }

    let tool_calls = tool_calls.finish()?;

    // Send usage update if available
    if let Some(mut usage_data) = usage {
        // Fill in context_length from model if not provided by API
        if usage_data.context_length == 0 {
            usage_data.context_length = get_context_length_for_model(model_settings.model);
        }
        let used_tokens = usage_data.input_tokens + usage_data.output_tokens;
        tracing::debug!(
            used = used_tokens,
            size = usage_data.context_length,
            "sending usage_update notification"
        );
        notify(session_notification(
            session_id.clone(),
            SessionUpdate::UsageUpdate(UsageUpdate::new(used_tokens, usage_data.context_length)),
        ))?;
    }

    Ok(ModelTurn {
        assistant_text,
        tool_calls,
        finish_reason,
        stop_reason,
        usage,
    })
}

/// Result of a single streamed model turn.
#[derive(Debug)]
pub(crate) struct ModelTurn {
    /// Aggregated assistant text from the stream.
    pub(crate) assistant_text: String,
    /// Fully assembled tool calls emitted by the model.
    pub(crate) tool_calls: Vec<ChatToolCall>,
    /// Raw finish reason reported by the LLM.
    pub(crate) finish_reason: FinishReason,
    /// ACP stop reason derived for the client.
    pub(crate) stop_reason: StopReason,
    /// Token usage for this sub-turn (accumulated across the prompt loop).
    pub(crate) usage: Option<UsageData>,
}

fn report_tool_call(
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

fn report_tool_result(
    session_id: &SessionId,
    notify: &mut impl FnMut(SessionNotification) -> Result<(), agent_client_protocol::Error>,
    call: &ChatToolCall,
    result: &ToolExecution,
) -> Result<(), AdapterError> {
    let mut fields = ToolCallUpdateFields::new()
        .status(result.status())
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

/// Build a human-readable display title for a tool call.
///
/// Extracts the most meaningful argument (path, command, pattern) and combines it with
/// the tool name to produce a title the client can render inline. Falls back to the
/// bare tool name when the arguments don't follow a recognised schema.
///
/// Examples:
/// - `run_command` + `{"command":"ls -la"}` → `"ls -la"`
/// - `read_file` + `{"path":"src/main.rs"}` → `"Read: src/main.rs"`
/// - `write_file` + `{"path":"Cargo.toml"}` → `"Write: Cargo.toml"`
/// - `edit_file` + `{"path":"src/lib.rs"}` → `"Edit: src/lib.rs"`
/// - `list_dir` + `{"path":"src/"}` → `"List: src/"`
/// - `grep` + `{"pattern":"fn main"}` → `"Search: fn main"`
/// - `glob` + `{"pattern":"*.rs"}` → `"Glob: *.rs"`
#[must_use]
pub(crate) fn tool_call_title(call: &ChatToolCall) -> String {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(call.arguments()) else {
        return call.name().to_string();
    };

    let Some(obj) = args.as_object() else {
        return call.name().to_string();
    };

    // Priority-ordered extraction: pick the most descriptive field present.
    let extracted = obj
        .get("command")
        .or_else(|| obj.get("pattern"))
        .or_else(|| obj.get("path"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match (call.name(), extracted) {
        ("update_plan", _) => "Update plan".to_string(),
        // For read/write/list/edit tools, prefix the path with an action verb
        // so the client can distinguish tool types at a glance.
        ("read_file", Some(path)) => format!("Read: {path}"),
        ("write_file", Some(path)) => format!("Write: {path}"),
        ("edit_file", Some(path)) => format!("Edit: {path}"),
        ("list_dir", Some(path)) => format!("List: {path}"),
        // For grep/glob, prefix with a search verb.
        ("grep", Some(pattern)) => format!("Search: {pattern}"),
        ("glob", Some(pattern)) => format!("Glob: {pattern}"),
        // run_command uses the command directly as the title — no prefix needed
        // since the command string is self-describing.
        ("run_command", Some(command)) => command.to_string(),
        // Fallback: use the extracted value if available, else just the tool name.
        (_, Some(value)) => value.to_string(),
        (name, None) => name.to_string(),
    }
}

/// Parse a tool call's raw JSON arguments for ACP notifications.
///
/// Invalid JSON is preserved as a plain string to keep notifications lossless.
#[must_use]
pub(crate) fn tool_raw_input(call: &ChatToolCall) -> serde_json::Value {
    serde_json::from_str(call.arguments())
        .unwrap_or_else(|_| serde_json::Value::String(call.arguments().to_string()))
}

/// Get the context window size for a model.
///
/// Returns the context window size in tokens. Falls back to `1_000_000` for unknown models.
/// Only `deepseek-chat` has an explicit override; GLM models fall through to the default.
/// See: <https://api-docs.deepseek.com/quick_start/pricing>
#[must_use]
fn get_context_length_for_model(model: &str) -> u64 {
    match model {
        "deepseek-chat" => 4_096,
        _ => 1_000_000,
    }
}

#[cfg(test)]
mod tests;
