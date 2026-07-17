use futures_util::StreamExt;
use serde::Deserialize;
use sse_reqwest_client::{EventSource, SseEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{DeepSeekError, FinishReason, StreamEvent, ToolCallDelta, UsageData};

/// Run a single SSE stream attempt, forwarding events into `tx`.
///
/// Returns when the stream completes, the cancellation token fires, or a
/// terminal error occurs. Errors are sent into `tx`; the caller does not
/// need to inspect the return value.
pub(super) async fn run_stream_attempt(
    mut event_source: EventSource,
    tx: &mpsc::UnboundedSender<Result<StreamEvent, DeepSeekError>>,
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
        let _ = tx.send(Err(DeepSeekError::InvalidResponse(
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
    #[serde(default, alias = "context_window")]
    context_length: u64,
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

pub(crate) fn parse_chat_completion_chunk(
    payload: &str,
) -> Result<Vec<StreamEvent>, DeepSeekError> {
    let chunk: ChatCompletionChunk = serde_json::from_str(payload)?;
    let Some(choice) = chunk.choices.into_iter().next() else {
        return Err(DeepSeekError::InvalidResponse(
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
                payload = %payload,
                "API chunk did not include context_length/context_window in usage"
            );
        }
        updates.push(StreamEvent::Usage(UsageData {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            context_length: usage.context_length,
        }));
    }

    Ok(updates)
}
