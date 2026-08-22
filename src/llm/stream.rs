use futures_util::StreamExt;
use serde::Deserialize;
use sse_reqwest_client::{EventSource, SseEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChatError, FinishReason, StreamEvent, ToolCallDelta, UsageData};

/// Run a single SSE stream attempt, forwarding events into `tx`.
///
/// Returns when the stream completes, the cancellation token fires, or a
/// terminal error occurs. Errors are sent into `tx`; the caller does not
/// need to inspect the return value.
pub(super) async fn run_stream_attempt(
    mut event_source: EventSource,
    tx: &mpsc::UnboundedSender<Result<StreamEvent, ChatError>>,
    cancellation_token: &CancellationToken,
) {
    let mut saw_finish = false;
    let mut events_sent: u32 = 0;

    loop {
        let event = tokio::select! {
            () = cancellation_token.cancelled() => return,
            event = event_source.next() => event,
        };

        let Some(event) = event else {
            break;
        };

        match event {
            Ok(SseEvent::Open) => {}
            Ok(SseEvent::Message(message)) => {
                let data = message.data.as_str();
                if data.trim() == "[DONE]" {
                    break;
                }
                match parse_chat_completion_chunk(data) {
                    Ok(updates) => {
                        for update in updates {
                            if matches!(update, StreamEvent::Finished(_)) {
                                saw_finish = true;
                            }
                            events_sent += 1;
                            if tx.send(Ok(update)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        return;
                    }
                }
            }
            Ok(SseEvent::Error(error)) => {
                tracing::warn!(error = ?error, events_sent, "SSE stream dropped; reconnecting");
            }
            Err(error) => {
                tracing::error!(error = ?error, events_sent, "terminal SSE stream error");
                let _ = tx.send(Err(error.into()));
                return;
            }
        }
    }

    if !saw_finish && !cancellation_token.is_cancelled() {
        let _ = tx.send(Err(ChatError::InvalidResponse(
            "stream ended before a finish reason was received".to_string(),
        )));
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCompletionUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default, alias = "context_window")]
    context_length: u64,
    #[serde(default)]
    prompt_tokens_details: PromptTokensDetails,
    #[serde(default)]
    completion_tokens_details: CompletionTokensDetails,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
}

/// Per-input token detail `DeepSeek` reports alongside the flat usage counters.
#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// Per-output token detail `DeepSeek` reports alongside the flat usage counters.
#[derive(Debug, Default, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    function: Option<ChatToolCallFunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

pub(crate) fn parse_chat_completion_chunk(payload: &str) -> Result<Vec<StreamEvent>, ChatError> {
    let chunk: ChatCompletionChunk = serde_json::from_str(payload)?;
    let Some(choice) = chunk.choices.into_iter().next() else {
        return Err(ChatError::InvalidResponse(
            "chat completion chunk did not include any choices".to_string(),
        ));
    };

    let mut updates = Vec::new();

    if let Some(reasoning) = choice
        .delta
        .reasoning_content
        .filter(|value| !value.is_empty())
    {
        updates.push(StreamEvent::Thought(reasoning));
    }

    if let Some(content) = choice.delta.content.filter(|value| !value.is_empty()) {
        updates.push(StreamEvent::Message(content));
    }

    for tool_call in choice.delta.tool_calls {
        updates.push(StreamEvent::ToolCallDelta(ToolCallDelta::new(
            tool_call.index,
            tool_call.id,
            tool_call
                .function
                .as_ref()
                .and_then(|function| function.name.clone()),
            tool_call.function.and_then(|function| function.arguments),
        )));
    }

    if let Some(finish_reason) = choice.finish_reason {
        updates.push(StreamEvent::Finished(FinishReason::from_api(
            &finish_reason,
        )));
    }

    if let Some(usage) = chunk.usage {
        tracing::debug!(
            input_tokens = usage.prompt_tokens,
            output_tokens = usage.completion_tokens,
            context_length = usage.context_length,
            "parsed usage data from API chunk"
        );
        if usage.context_length == 0 {
            tracing::debug!(
                "API chunk did not include context_length/context_window in usage; \
                 falling back to the model context-window table"
            );
        }
        updates.push(StreamEvent::Usage(UsageData {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            context_length: usage.context_length,
            total_tokens: usage.total_tokens,
            thought_tokens: usage.completion_tokens_details.reasoning_tokens,
            // DeepSeek reports cache hits twice: once as the OpenAI-style
            // `prompt_tokens_details.cached_tokens` and once as the flat
            // `prompt_cache_hit_tokens`. Prefer the structured field when
            // both are present and fall back to the flat counter otherwise.
            cached_read_tokens: usage
                .prompt_tokens_details
                .cached_tokens
                .or(usage.prompt_cache_hit_tokens),
            // Tokens that missed the prompt cache are newly written to it,
            // so they map to ACP's "cache write" counter.
            cached_write_tokens: usage.prompt_cache_miss_tokens,
        }));
    }

    Ok(updates)
}
