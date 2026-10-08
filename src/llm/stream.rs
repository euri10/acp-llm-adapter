use futures_util::StreamExt;
use serde::Deserialize;
use sse_reqwest_client::{Error as SseError, EventSource, SseErrorEvent, SseEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChatError, FinishReason, StreamEvent, ToolCallDelta, UsageData};

/// Forward an SSE completion into `tx`, recovering only before any event.
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
    let mut emitted_event = false;

    loop {
        let event = tokio::select! {
            biased;
            () = cancellation_token.cancelled() => return,
            () = tx.closed() => return,
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
                            if saw_finish && !matches!(update, StreamEvent::Usage(_)) {
                                let _ = tx.send(Err(ChatError::InvalidResponse(
                                    "generation event received after a finish reason".to_string(),
                                )));
                                return;
                            }
                            if matches!(update, StreamEvent::Finished(_)) {
                                saw_finish = true;
                            }
                            emitted_event = true;
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
                if saw_finish && matches!(error, SseErrorEvent::Eof) {
                    // The terminal finish reason completes the generation;
                    // providers may close without a separate [DONE] marker.
                    return;
                }
                if emitted_event {
                    let _ = tx.send(Err(ChatError::Transport(Box::new(error))));
                    return;
                }
                tracing::warn!(error = ?error, "SSE stream dropped before output; reconnecting");
            }
            Ok(SseEvent::Discarded(error)) => {
                // Dropping a delta can leave different but valid tool arguments.
                // Fail the completion and drop its owned source before any replay.
                let _ = tx.send(Err(SseError::PayloadTooLarge(error).into()));
                return;
            }
            Err(error) => {
                if saw_finish && matches!(error, SseError::Timeout(_, SseErrorEvent::Eof)) {
                    // Retry-disabled streams report EOF as a terminal error,
                    // even when the completion already had its finish reason.
                    return;
                }
                tracing::error!(error = ?error, emitted_event, "terminal SSE stream error");
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
    // Groq documents both envelopes, including usage on the final chunk:
    // https://github.com/groq/groq-python/blob/main/src/groq/types/chat/chat_completion_chunk.py
    #[serde(default)]
    x_groq: Option<GroqMetadata>,
}

#[derive(Debug, Deserialize)]
struct GroqMetadata {
    usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCompletionUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default, alias = "context_window")]
    context_length: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
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

impl ChatCompletionUsage {
    fn normalize(self) -> Result<UsageData, ChatError> {
        let (Some(input_tokens), Some(output_tokens)) =
            (self.prompt_tokens, self.completion_tokens)
        else {
            return Err(ChatError::InvalidResponse(
                "usage must include input and output token counts".into(),
            ));
        };
        let usage = UsageData {
            input_tokens,
            output_tokens,
            context_length: self.context_length,
            total_tokens: self.total_tokens,
            thought_tokens: self
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
            // DeepSeek may repeat cache hits in its flat and structured fields.
            cached_read_tokens: merge_counter(
                self.prompt_tokens_details
                    .and_then(|details| details.cached_tokens),
                self.prompt_cache_hit_tokens,
            )?,
            cached_write_tokens: self.prompt_cache_miss_tokens,
        };
        usage.validated_total_tokens()?;
        Ok(usage)
    }
}

fn merge_counter(left: Option<u64>, right: Option<u64>) -> Result<Option<u64>, ChatError> {
    if matches!((left, right), (Some(left), Some(right)) if left != right) {
        return Err(ChatError::InvalidResponse(
            "conflicting usage envelopes".into(),
        ));
    }
    Ok(left.or(right))
}

/// Both envelopes describe one completion, never separate billable work.
fn normalize_usage(
    top: Option<ChatCompletionUsage>,
    groq: Option<GroqMetadata>,
) -> Result<Option<UsageData>, ChatError> {
    let top = top.map(ChatCompletionUsage::normalize).transpose()?;
    let groq = groq
        .and_then(|metadata| metadata.usage)
        .map(ChatCompletionUsage::normalize)
        .transpose()?;
    let (Some(left), Some(right)) = (top, groq) else {
        return Ok(top.or(groq));
    };
    if left.input_tokens != right.input_tokens || left.output_tokens != right.output_tokens {
        return Err(ChatError::InvalidResponse(
            "conflicting usage envelopes".into(),
        ));
    }
    let usage = UsageData {
        input_tokens: left.input_tokens,
        output_tokens: left.output_tokens,
        context_length: merge_counter(
            (left.context_length != 0).then_some(left.context_length),
            (right.context_length != 0).then_some(right.context_length),
        )?
        .unwrap_or(0),
        total_tokens: merge_counter(left.total_tokens, right.total_tokens)?,
        thought_tokens: merge_counter(left.thought_tokens, right.thought_tokens)?,
        cached_read_tokens: merge_counter(left.cached_read_tokens, right.cached_read_tokens)?,
        cached_write_tokens: merge_counter(left.cached_write_tokens, right.cached_write_tokens)?,
    };
    usage.validated_total_tokens()?;
    Ok(Some(usage))
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
    let usage = normalize_usage(chunk.usage, chunk.x_groq)?;
    let Some(choice) = chunk.choices.into_iter().next() else {
        return usage
            .map(|usage| vec![StreamEvent::Usage(usage)])
            .ok_or_else(|| {
                ChatError::InvalidResponse(
                    "chat completion chunk did not include any choices".to_string(),
                )
            });
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

    if let Some(usage) = usage {
        tracing::debug!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            context_length = usage.context_length,
            "parsed usage data from API chunk"
        );
        if usage.context_length == 0 {
            tracing::debug!(
                "API chunk did not include context_length/context_window in usage; \
                 falling back to the model context-window table"
            );
        }
        updates.push(StreamEvent::Usage(usage));
    }

    Ok(updates)
}
