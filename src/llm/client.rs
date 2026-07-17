use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::Client as HttpClient;
use sse_reqwest_client::RequestBuilderExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::stream::run_stream_attempt;
use super::types::{ChatRequest, WireMessage, WireToolDefinition};
use super::{ChatConfig, ChatError, StreamEvent};

/// A `DeepSeek` chat-completions client.
///
/// The client implements [`LlmClient`] and streams normalized
/// [`StreamEvent`] values from `DeepSeek`'s OpenAI-compatible chat endpoint.
///
/// # Examples
///
/// ```rust
/// use acp_llm_adapter::llm::{ChatClient, ChatConfig};
///
/// let client = ChatClient::new(ChatConfig::new(
///     "test-key",
///     "https://api.deepseek.com",
///     "deepseek-v4-pro",
/// ));
///
/// assert_eq!(client.config().model(), "deepseek-v4-pro");
/// ```
#[derive(Debug, Clone)]
pub struct ChatClient {
    http: HttpClient,
    config: ChatConfig,
}

impl ChatClient {
    /// Build a client from explicit configuration.
    #[must_use]
    pub fn new(config: ChatConfig) -> Self {
        Self {
            http: HttpClient::new(),
            config,
        }
    }

    /// Build a client from process environment.
    ///
    /// # Errors
    ///
    /// Returns `MissingApiKey` when the required key is absent or empty.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use acp_llm_adapter::llm::ChatClient;
    ///
    /// let client = ChatClient::from_env()?;
    /// assert!(!client.config().base_url().is_empty());
    /// # Ok::<(), acp_llm_adapter::llm::ChatError>(())
    /// ```
    pub fn from_env() -> Result<Self, ChatError> {
        Ok(Self::new(ChatConfig::from_env()?))
    }

    /// Return the client configuration.
    #[must_use]
    pub fn config(&self) -> &ChatConfig {
        &self.config
    }

    /// Fetch the list of available model IDs from the provider's `/models` endpoint.
    ///
    /// Delegates to the free function [`fetch_available_models`].
    pub async fn fetch_available_models(&self, preferred_default: &str) -> Vec<String> {
        fetch_available_models(
            self.config.base_url(),
            self.config.api_key(),
            preferred_default,
        )
        .await
    }
}

/// Fetch model IDs from an OpenAI-compatible `GET /models` endpoint.
///
/// `preferred_default` is placed first in the returned list. On any failure
/// (transport, auth, parse) the function logs a warning and returns
/// `vec![preferred_default.to_string()]` so callers can always proceed.
pub async fn fetch_available_models(
    base_url: &str,
    api_key: &str,
    preferred_default: &str,
) -> Vec<String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let http = HttpClient::new();

    let response = match http.get(&url).bearer_auth(api_key).send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                %err,
                %url,
                "failed to fetch /models; falling back to default model list"
            );
            return vec![preferred_default.to_string()];
        }
    };

    let body: serde_json::Value = match response.json().await {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(
                %err,
                %url,
                "failed to parse /models response; falling back to default model list"
            );
            return vec![preferred_default.to_string()];
        }
    };

    let mut models: Vec<String> = body
        .get("data")
        .and_then(|data| data.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("id")?.as_str().map(String::from))
        .collect();

    // Ensure the preferred default is present and first.
    models.retain(|id| id != preferred_default);
    models.sort();
    let mut result = vec![preferred_default.to_string()];
    result.append(&mut models);
    result
}

/// A client abstraction for streaming chat-completions turns.
pub trait LlmClient: Send + Sync {
    /// Stream a turn and yield normalized reasoning, text, and terminal events.
    ///
    /// The stream should stop promptly when `cancellation_token` is cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be constructed or the transport fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use acp_llm_adapter::llm::{ChatMessage, ChatRequest, ChatClient, LlmClient};
    /// use futures_util::StreamExt;
    /// use tokio_util::sync::CancellationToken;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let client = ChatClient::from_env()?;
    ///     let request = ChatRequest::new(vec![ChatMessage::user("Say hello")]);
    ///     let mut stream = client.stream_chat(request, CancellationToken::new())?;
    ///
    ///     while let Some(event) = stream.next().await {
    ///         let _ = event?;
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ChatError>>, ChatError>;
}

impl LlmClient for ChatClient {
    fn stream_chat(
        &self,
        request: ChatRequest,
        cancellation_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ChatError>>, ChatError> {
        if self.config.api_key().trim().is_empty() {
            return Err(ChatError::MissingApiKey);
        }

        let (messages, tools, model_opt, reasoning_effort, max_tokens) = request.into_parts();
        let model = model_opt.unwrap_or_else(|| self.config.model().to_string());
        let wire_messages: Vec<WireMessage> = messages
            .into_iter()
            .map(|message| WireMessage::from(&message))
            .collect();
        let wire_tools: Vec<WireToolDefinition> =
            tools.iter().map(WireToolDefinition::from).collect();

        // Build the body using a map so that empty/null fields are omitted
        // (matching the previous `#[serde(skip_serializing_if = "...")]` behavior).
        let mut fields = serde_json::Map::new();
        fields.insert("model".to_string(), serde_json::json!(model));
        fields.insert("messages".to_string(), serde_json::json!(wire_messages));
        fields.insert("stream".to_string(), serde_json::json!(true));
        if !wire_tools.is_empty() {
            fields.insert("tools".to_string(), serde_json::json!(wire_tools));
        }
        if let Some(ref effort) = reasoning_effort {
            fields.insert("reasoning_effort".to_string(), serde_json::json!(effort));
        }
        if let Some(ref tokens) = max_tokens {
            fields.insert("max_tokens".to_string(), serde_json::json!(tokens));
        }
        let body = serde_json::Value::Object(fields);

        let http = self.http.clone();
        let url = format!(
            "{}/chat/completions",
            self.config.base_url().trim_end_matches('/')
        );
        let api_key = self.config.api_key().to_string();

        let (tx, rx) = mpsc::unbounded_channel::<Result<StreamEvent, ChatError>>();

        tokio::spawn(async move {
            let event_source = http
                .post(&url)
                .bearer_auth(&api_key)
                .json(&body)
                .into_event_source();

            tracing::debug!(
                url = %url,
                model = %model,
                message_count = wire_messages.len(),
                tool_count = wire_tools.len(),
                stream = true,
                reasoning_effort = ?reasoning_effort,
                max_tokens = ?max_tokens,
                "sending chat completion request to DeepSeek"
            );

            if tracing::enabled!(tracing::Level::TRACE) {
                // Serialize the full body for trace-level debugging
                if let Ok(request_json) = serde_json::to_string(&body) {
                    tracing::trace!(request_body = %request_json, "DeepSeek request body");
                }
            }

            run_stream_attempt(event_source, &tx, &cancellation_token).await;
        });

        Ok(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed())
    }
}
