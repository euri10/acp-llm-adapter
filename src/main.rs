//! Command-line entrypoint for the `acp-llm-adapter`.

#![forbid(unsafe_code)]
#![deny(
    warnings,
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]
// `#[must_use]` on every internal binary helper is noise at this stage.
#![allow(clippy::must_use_candidate)]

use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use acp_llm_adapter::logsink::{ConnectionLog, Direction, KIND_TRACE, LogRecord, LogSink};
use acp_llm_adapter::paths::default_state_dir;
use agent_client_protocol::{Agent, ConnectTo, Lines};
use tokio_util::sync::CancellationToken;

use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::{ChatConfig, FinishReason};
use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, ContentBlock, EmbeddedResourceResource,
    SessionNotification, SessionUpdate, StopReason, UnstructuredCommandInput,
};
use clap::{Parser, Subcommand};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use uuid::Uuid;

mod acp;
mod dev;
mod mcp;
mod session;
mod session_store;
#[cfg(test)]
mod test_utils;
mod tools;
mod turn;

pub(crate) use acp::{
    PermissionRequester, ReadTextFileRequester, TerminalRequester, ToolCallRequester,
    WriteTextFileRequester, serve_with_transport_and_state_dir_logging,
};
pub(crate) use dev::{
    Backend, build_dev_agent, exercise_permission_gate_smoke, llm_client_for_backend,
    print_dev_smoke_result, run_smoke_flow,
};
pub(crate) use mcp::{
    McpSession, connect_mcp_sessions, is_mcp_tool_name, mcp_tool_execution, mcp_tool_kind,
};
pub(crate) use session_store::FilesystemSessionStore;
use tools::AdapterToolRegistry;
pub(crate) use turn::tool_raw_input;

// Re-export session domain types so other modules can use `crate::*` imports.
pub(crate) use session::{
    AdapterState, DEFAULT_MAX_TURN_REQUESTS, PendingToolCalls, PermissionDecision, ReasoningEffort,
    SESSION_CONFIG_MAX_TOKENS_ID, SESSION_CONFIG_MODE_ID, SESSION_CONFIG_MODEL_ID,
    SESSION_CONFIG_REASONING_EFFORT_ID, SessionBehavior, SessionRecord, SessionStore,
    default_session_modes, derive_session_title, initial_model, iso_timestamp_now,
    max_tokens_from_value_id, request_tool_permission, session_modes, validate_session_model,
};

const ADAPTER_NAME: &str = env!("CARGO_PKG_NAME");
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");
const ACP_LOG_ENV: &str = "ACP_LOG";
const LOG_QUEUE_CAPACITY: usize = 1024;

/// Returns the list of available slash commands for the ACP LLM adapter.
///
/// These commands are advertised to the client via `AvailableCommandsUpdate`
/// after session creation, letting users invoke common workflows.
#[must_use]
fn adapter_available_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new("explain", "Explain selected code or a concept in detail").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "The code or concept to explain",
            )),
        ),
        AvailableCommand::new("fix", "Identify and fix issues in the selected code").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "The code with issues to fix",
            )),
        ),
        AvailableCommand::new("test", "Generate tests for the selected code").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "The code to generate tests for",
            )),
        ),
        AvailableCommand::new(
            "search",
            "Search the codebase for relevant code or documentation",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("The search query or keywords"),
        )),
        AvailableCommand::new("clear", "Clear the conversation history and start fresh"),
    ]
}

#[derive(Debug, Parser)]
#[command(
    name = "acp-llm-adapter",
    version,
    about = "ACP stdio adapter for LLM-backed coding sessions (DeepSeek, GLM)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run the ACP server over standard input and output.
    Serve {
        #[arg(long, value_enum)]
        backend: Backend,
        /// Maximum tool-call/response cycles per prompt turn (must be ≥ 1).
        #[arg(long, default_value_t = DEFAULT_MAX_TURN_REQUESTS)]
        max_turn_requests: NonZeroUsize,
    },
    #[command(hide = true)]
    Dev {
        #[arg(long, value_enum)]
        backend: Backend,
        #[arg(long, default_value = "Hello from the dev smoke test.")]
        prompt: String,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AdapterError> {
    let command = Cli::parse().command;
    if matches!(command, Command::Dev { .. }) {
        init_tracing(None)?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        match command {
            Command::Serve {
                backend,
                max_turn_requests,
            } => serve(backend, max_turn_requests).await,
            Command::Dev { backend, prompt } => dev(backend, prompt).await,
        }
    })?;

    Ok(())
}

fn init_tracing(connection: Option<ConnectionLog>) -> Result<(), AdapterError> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(SessionLogLayer::new(connection))
        .try_init()
        .map_err(|e| AdapterError::Internal(e.to_string()))?;
    Ok(())
}

#[derive(Debug)]
struct SessionLogLayer {
    connection: Option<ConnectionLog>,
}

impl SessionLogLayer {
    fn new(connection: Option<ConnectionLog>) -> Self {
        Self { connection }
    }

    fn route(
        &self,
        session_id: Option<&str>,
        level: &str,
        target: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) {
        let Some(connection) = &self.connection else {
            return;
        };
        let payload = serde_json::json!({
            "level": level,
            "target": target,
            "fields": fields,
        });
        let record = LogRecord::new(Direction::Internal, KIND_TRACE, payload);
        if let Some(session_id) = session_id {
            connection.log(record.with_session(session_id));
        } else {
            connection.log_fallback(record);
        }
    }
}

#[derive(Debug, Default)]
struct SessionField {
    session_id: Option<String>,
}

#[derive(Debug)]
struct EventFields {
    values: serde_json::Map<String, serde_json::Value>,
}

impl Default for EventFields {
    fn default() -> Self {
        Self {
            values: serde_json::Map::new(),
        }
    }
}

impl tracing::field::Visit for SessionField {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "session_id" {
            self.session_id = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "session_id" {
            self.session_id = Some(value.to_string());
        }
    }
}

impl tracing::field::Visit for EventFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.values.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{value:?}")),
        );
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.values.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.values.insert(field.name().to_string(), value.into());
    }
}

impl<S> Layer<S> for SessionLogLayer
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut fields = SessionField::default();
        attrs.record(&mut fields);
        if let Some(session_id) = fields.session_id
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(session_id);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let session_id = ctx
            .event_scope(event)
            .and_then(|mut scope| scope.find_map(|span| span.extensions().get::<String>().cloned()))
            .filter(|session_id| session_id != "none");
        let mut fields = EventFields::default();
        event.record(&mut fields);
        self.route(
            session_id.as_deref(),
            event.metadata().level().as_str(),
            event.metadata().target(),
            &fields.values,
        );
    }
}

/// Cancels its [`CancellationToken`] when dropped.
///
/// The ACP transport reads incoming JSON-RPC lines from stdin and drops the
/// incoming stream once it reaches end-of-input. By moving an `EofGuard` into
/// the incoming stream's adapter closure (see [`stdio_transport_with_eof`]),
/// dropping that stream at stdin EOF cancels the token, which the serve loop
/// races on so the process shuts down promptly instead of hanging forever.
#[derive(Debug)]
struct EofGuard {
    token: CancellationToken,
}

// A plain RAII cancellation signal: `Drop` only flips a `CancellationToken`. This
// is not the "manual Drop manipulation" AGENTS.md §5.2 warns about (no `unsafe`,
// no resource juggling) — it is the idiomatic way to fire a signal when a value
// goes out of scope.
impl Drop for EofGuard {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// Wrap `stream` so that `token` is cancelled once the stream is dropped.
///
/// The returned stream owns an [`EofGuard`]. The ACP transport drops the
/// incoming line stream when stdin reaches EOF, which drops the guard and
/// cancels the token.
fn attach_eof_guard<S, T>(
    stream: S,
    token: CancellationToken,
) -> impl futures_util::Stream<Item = T>
where
    S: futures_util::Stream<Item = T>,
{
    use futures_util::StreamExt;

    let guard = EofGuard { token };
    stream.map(move |item| {
        // Keep `guard` owned by the stream: dropping the stream drops the guard.
        let _guard = &guard;
        item
    })
}

/// Build the stdio ACP transport, cancelling `shutdown` when stdin reaches EOF.
///
/// Mirrors the line-mode transport that `agent_client_protocol::Stdio` builds,
/// but attaches an [`EofGuard`] to the incoming line stream so the serve loop
/// can detect client disconnect (stdin close) and exit instead of hanging,
/// because the ACP agent server otherwise runs forever.
fn stdio_transport_with_eof(
    shutdown: CancellationToken,
    connection: Option<ConnectionLog>,
) -> impl ConnectTo<Agent> + 'static {
    use futures_util::io::BufReader;
    use futures_util::{AsyncBufReadExt, AsyncWriteExt, StreamExt};

    let stdin = blocking::Unblock::new(std::io::stdin());
    let stdout = blocking::Unblock::new(std::io::stdout());

    let incoming_log = connection.clone();
    let incoming = attach_eof_guard(BufReader::new(stdin).lines(), shutdown).map(move |line| {
        if let (Some(log), Ok(line)) = (&incoming_log, &line) {
            log.log(LogRecord::frame(Direction::ClientToAgent, line));
        }
        line
    });

    let outgoing_log = connection;
    let outgoing = futures_util::sink::unfold(stdout, move |mut writer, line: String| {
        let log = outgoing_log.clone();
        async move {
            if let Some(log) = log {
                bind_session_from_frame(&log, &line);
                log.log(LogRecord::frame(Direction::AgentToClient, &line));
            }
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            Ok::<_, std::io::Error>(writer)
        }
    });

    Lines::new(outgoing, incoming)
}

fn bind_session_from_frame(connection: &ConnectionLog, line: &str) {
    let Ok(frame) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(session_id) = frame
        .get("result")
        .and_then(|result| result.get("sessionId"))
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    if let Err(error) = connection.bind_session(session_id) {
        tracing::warn!(%error, "failed to bind serve log to ACP session");
    }
}

fn serve_log() -> Result<Option<Arc<LogSink>>, agent_client_protocol::Error> {
    let enabled =
        std::env::var_os(ACP_LOG_ENV).is_some_and(|value| !value.is_empty() && value != "0");
    if !enabled {
        return Ok(None);
    }

    let Some(state_dir) = default_state_dir() else {
        return Err(agent_client_protocol::Error::internal_error()
            .data("ACP_LOG is enabled but neither XDG_STATE_HOME nor HOME is set"));
    };
    LogSink::spawn(state_dir, LOG_QUEUE_CAPACITY)
        .map(Some)
        .map_err(agent_client_protocol::Error::into_internal_error)
}

/// Resolve when the process receives `SIGTERM`, `SIGINT`, or `SIGHUP`.
///
/// # Errors
///
/// Returns an internal ACP error if a signal listener cannot be registered.
#[cfg(unix)]
async fn shutdown_signal() -> Result<(), agent_client_protocol::Error> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let mut sighup =
        signal(SignalKind::hangup()).map_err(agent_client_protocol::Error::into_internal_error)?;

    let which = tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
        _ = sighup.recv() => "SIGHUP",
    };
    tracing::info!(signal = which, "received termination signal");
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<(), agent_client_protocol::Error> {
    tokio::signal::ctrl_c()
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    tracing::info!(signal = "CTRL_C", "received termination signal");
    Ok(())
}

#[tracing::instrument(name = "acp_serve", skip_all, fields(session_id = "none"))]
async fn serve(
    backend: Backend,
    max_turn_requests: NonZeroUsize,
) -> Result<(), agent_client_protocol::Error> {
    let log_sink = serve_log()?;
    let connection = log_sink
        .as_ref()
        .map(|sink| sink.connection(Uuid::new_v4().to_string()))
        .transpose()
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    init_tracing(connection.clone()).map_err(agent_client_protocol::Error::into_internal_error)?;

    let llm_client = llm_client_for_backend(backend)?;
    let tool_registry = Arc::new(AdapterToolRegistry);

    let default_model = initial_model(backend.default_model());
    let state = Arc::new(Mutex::new(AdapterState::new(default_model.clone())));

    // Fetch the live model list from the provider. Uses the backend's known
    // base URL and the API key from the process environment so the endpoint
    // always matches the provider (DeepSeek → api.deepseek.com, GLM → api.z.ai).
    if backend != Backend::Mock {
        let api_key = std::env::var(ChatConfig::ENV_API_KEY)
            .ok()
            .filter(|v| !v.trim().is_empty());

        if let Some(ref key) = api_key {
            let models = acp_llm_adapter::llm::fetch_available_models(
                backend.default_base_url(),
                key,
                &default_model,
            )
            .await;
            if let Err(e) = state.lock().map(|mut g| g.set_available_models(models)) {
                tracing::warn!(%e, "failed to store fetched model list");
            }
        }
    }

    let shutdown = CancellationToken::new();
    let logging_enabled = connection.is_some();
    let transport = stdio_transport_with_eof(shutdown.clone(), connection);

    let result = tokio::select! {
        result = serve_with_transport_and_state_dir_logging(
            transport,
            state,
            llm_client,
            tool_registry,
            max_turn_requests,
            None,
            logging_enabled,
        ) => {
            tracing::info!("ACP serve loop returned");
            result
        }
        () = shutdown.cancelled() => {
            tracing::info!("stdin EOF detected; shutting down");
            Ok(())
        }
        result = shutdown_signal() => {
            tracing::info!("termination signal received; shutting down");
            result
        }
    };
    if let Some(sink) = log_sink {
        sink.flush();
    }
    result
}

async fn dev(backend: Backend, prompt: String) -> Result<(), agent_client_protocol::Error> {
    let agent = build_dev_agent(
        &std::env::current_exe().map_err(|error| {
            agent_client_protocol::Error::internal_error()
                .data(format!("failed to locate current executable: {error}"))
        })?,
        backend,
    )?;
    let result = run_smoke_flow(agent, prompt).await?;
    print_dev_smoke_result(&result);
    exercise_permission_gate_smoke().await?;
    Ok(())
}

fn text_from_prompt(prompt: &[ContentBlock]) -> Result<String, AdapterError> {
    let mut text = String::new();

    for block in prompt {
        match block {
            ContentBlock::Text(content) => text.push_str(&content.text),
            ContentBlock::ResourceLink(link) => text.push_str(&resource_link_prompt_text(link)),
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(contents) => {
                    text.push_str(&resource_text_prompt_text(contents));
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err(AdapterError::InvalidParams(
                        "binary resource prompt blocks are not supported".into(),
                    ));
                }
                _ => {
                    return Err(AdapterError::InvalidParams(
                        "unsupported embedded resource prompt block".into(),
                    ));
                }
            },
            _ => {
                return Err(AdapterError::InvalidParams(
                    "only text, resource link, and text resource prompt blocks are supported"
                        .into(),
                ));
            }
        }
    }

    if text.trim().is_empty() {
        return Err(AdapterError::InvalidParams(
            "prompt must include non-empty text".into(),
        ));
    }

    Ok(text)
}

fn resource_link_prompt_text(link: &agent_client_protocol::schema::v1::ResourceLink) -> String {
    let display_name = link.title.as_deref().unwrap_or(link.name.as_str());
    let mut rendered = String::new();
    rendered.push_str("[resource] ");
    rendered.push_str(display_name);
    rendered.push_str(" <");
    rendered.push_str(&link.uri);
    rendered.push('>');

    if let Some(description) = &link.description {
        rendered.push_str(" - ");
        rendered.push_str(description);
    }

    rendered
}

fn resource_text_prompt_text(
    contents: &agent_client_protocol::schema::v1::TextResourceContents,
) -> String {
    let mut rendered = String::new();
    rendered.push_str("[resource] <");
    rendered.push_str(&contents.uri);
    rendered.push_str(">\n");
    rendered.push_str(&contents.text);
    rendered
}

fn session_notification(
    session_id: agent_client_protocol::schema::v1::SessionId,
    update: SessionUpdate,
) -> SessionNotification {
    SessionNotification::new(session_id, update)
}

fn stop_reason_from_finish(reason: &FinishReason) -> StopReason {
    match reason {
        FinishReason::EndTurn | FinishReason::ToolCalls | FinishReason::Other(_) => {
            StopReason::EndTurn
        }
        FinishReason::MaxTokens => StopReason::MaxTokens,
        FinishReason::Refusal => StopReason::Refusal,
    }
}

/// Create a `SessionStore` backed by a fresh default adapter state.
///
/// This is a convenience for tests that previously created
/// `Arc<Mutex<AdapterState>>` directly.
#[cfg(test)]
pub(crate) fn test_store() -> SessionStore {
    let mut state = AdapterState::default();
    // Seed the known model list so tests that exercise model switching work.
    state.set_available_models(vec![
        "deepseek-v4-pro".to_string(),
        "deepseek-v4-flash".to_string(),
    ]);
    SessionStore::new(Arc::new(Mutex::new(state)))
}

#[cfg(test)]
// Test assertions legitimately use indexing to access elements by position; replacing
// every `slice[i]` with `.get(i).unwrap()` adds noise without safety benefit in tests.
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::{
        Backend, Cli, Command, EofGuard, LogSink, SessionLogLayer, Uuid, attach_eof_guard,
        bind_session_from_frame, text_from_prompt,
    };
    use crate::acp::validate_session_paths;
    use agent_client_protocol::schema::v1::{
        BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
        ImageContent, NewSessionRequest, ResourceLink, StopReason, TextResourceContents,
    };
    use clap::Parser;
    use futures_util::StreamExt;
    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::prelude::*;

    #[test_log::test]
    fn rejects_serve_subcommand_without_backend() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "serve"]);
        assert!(
            parsed.is_err(),
            "expected parse failure for missing required backend"
        );
        let message = parsed.err().map_or_else(String::new, |e| e.to_string());
        assert!(message.contains("required") || message.contains("--backend"));
    }

    #[test_log::test]
    fn parses_dev_subcommand() {
        let parsed = Cli::try_parse_from([
            "acp-llm-adapter",
            "dev",
            "--backend",
            "mock",
            "--prompt",
            "smoke",
        ]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Dev {
                    backend: Backend::Mock,
                    prompt,
                }
            }) if prompt == "smoke"
        ));
    }

    #[test_log::test]
    fn rejects_dev_subcommand_without_backend() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "dev"]);
        assert!(
            parsed.is_err(),
            "expected parse failure for missing required backend"
        );
        let message = parsed.err().map_or_else(String::new, |e| e.to_string());
        assert!(message.contains("required") || message.contains("--backend"));
    }

    #[test_log::test]
    fn helper_validation_and_prompt_error_branches() -> Result<(), agent_client_protocol::Error> {
        assert_eq!(
            text_from_prompt(&[ContentBlock::from("hello"), ContentBlock::from(" world")])?,
            "hello world"
        );

        let resource_link_prompt = vec![ContentBlock::ResourceLink(ResourceLink::new(
            "docs",
            "file:///docs/reference.md",
        ))];
        assert_eq!(
            text_from_prompt(&resource_link_prompt)?,
            "[resource] docs <file:///docs/reference.md>"
        );

        let text_resource_prompt = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "context body",
                "file:///docs/context.md",
            )),
        ))];
        assert_eq!(
            text_from_prompt(&text_resource_prompt)?,
            "[resource] <file:///docs/context.md>\ncontext body"
        );

        let blob_resource_prompt = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(
                "aGVsbG8=",
                "file:///docs/context.bin",
            )),
        ))];
        let Err(error) = text_from_prompt(&blob_resource_prompt) else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("expected binary resource prompt to fail"));
        };
        assert!(
            error
                .to_string()
                .contains("binary resource prompt blocks are not supported")
        );

        let image_prompt = vec![ContentBlock::Image(ImageContent::new(
            "aGVsbG8=",
            "image/png",
        ))];
        let Err(error) = text_from_prompt(&image_prompt) else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("expected image prompt to fail"));
        };
        assert!(
            error.to_string().contains(
                "only text, resource link, and text resource prompt blocks are supported"
            )
        );

        let Err(error) = text_from_prompt(&[]) else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("expected empty prompt to fail"));
        };
        assert!(
            error
                .to_string()
                .contains("prompt must include non-empty text")
        );

        let relative_request = NewSessionRequest::new("relative");
        let Err(error) = validate_session_paths(&relative_request) else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("expected relative cwd to fail"));
        };
        assert!(
            error
                .to_string()
                .contains("session cwd must be an absolute path")
        );

        let relative_additional = NewSessionRequest::new("/tmp")
            .additional_directories(vec![std::path::PathBuf::from("relative")]);
        let Err(error) = validate_session_paths(&relative_additional) else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("expected relative additional directory to fail"));
        };
        assert!(
            error
                .to_string()
                .contains("additional session directories must be absolute paths")
        );

        Ok(())
    }

    #[test]
    fn resource_link_prompt_includes_description_when_present() {
        use super::resource_link_prompt_text;
        let mut link = ResourceLink::new("docs", "file:///ref.md");
        link.description = Some("Reference docs".to_string());
        let rendered = resource_link_prompt_text(&link);
        assert!(rendered.contains("Reference docs"));
        assert!(rendered.contains(" - "));
    }

    #[test]
    fn resource_link_prompt_text_uses_title_over_name() {
        use super::resource_link_prompt_text;
        let mut link = ResourceLink::new("internal_name", "file:///foo.md");
        link.title = Some("Display Title".to_string());
        let rendered = resource_link_prompt_text(&link);
        assert!(rendered.contains("Display Title"));
        assert!(!rendered.contains("internal_name"));
    }

    #[test]
    fn adapter_available_commands_returns_five_commands() {
        use super::adapter_available_commands;
        let commands = adapter_available_commands();
        assert_eq!(commands.len(), 5);

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["explain", "fix", "test", "search", "clear"]);
    }

    #[test]
    fn adapter_available_commands_input_fields_are_correct() {
        use super::adapter_available_commands;
        let commands = adapter_available_commands();

        // Commands with input fields: explain, fix, test, search
        for name in &["explain", "fix", "test", "search"] {
            let cmd = commands.iter().find(|c| c.name == *name);
            assert!(cmd.is_some(), "command '{name}' missing");
            assert!(
                cmd.and_then(|c| c.input.as_ref()).is_some(),
                "command '{name}' should have an input field"
            );
        }

        // clear has no input field
        let clear = commands.iter().find(|c| c.name == "clear");
        assert!(clear.is_some(), "clear command missing");
        assert!(
            clear.and_then(|c| c.input.as_ref()).is_none(),
            "clear command should have no input field"
        );
    }

    #[test]
    fn session_notification_creates_correct_notification() {
        use super::session_notification;
        use agent_client_protocol::schema::v1::{CurrentModeUpdate, SessionId, SessionUpdate};

        let session_id = SessionId::new("test-session");
        let update = SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("chat"));
        let notification = session_notification(session_id.clone(), update);

        assert_eq!(notification.session_id, session_id);
        assert!(matches!(
            notification.update,
            SessionUpdate::CurrentModeUpdate(_)
        ));
    }

    #[test]
    fn resource_text_prompt_text_renders_uri_and_content() {
        use super::resource_text_prompt_text;
        use agent_client_protocol::schema::v1::TextResourceContents;

        let contents = TextResourceContents::new("file body", "file:///tmp/notes.txt");
        let rendered = resource_text_prompt_text(&contents);
        assert_eq!(rendered, "[resource] <file:///tmp/notes.txt>\nfile body");
    }

    #[test]
    fn cli_rejects_invalid_subcommand() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "bogus"]);
        assert!(
            parsed.is_err(),
            "expected parse failure for invalid subcommand"
        );
        // clap should indicate the unrecognized subcommand
        let message = parsed.err().map_or_else(String::new, |e| e.to_string());
        assert!(message.contains("bogus") || message.contains("unrecognized"));
    }

    #[test]
    fn cli_rejects_invalid_backend_for_serve() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "serve", "--backend", "invalid"]);
        assert!(
            parsed.is_err(),
            "expected parse failure for invalid backend"
        );
        let message = parsed.err().map_or_else(String::new, |e| e.to_string());
        assert!(message.contains("invalid") || message.contains("backend"));
    }

    #[test]
    fn cli_rejects_invalid_backend_for_dev() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "dev", "--backend", "bogus"]);
        assert!(
            parsed.is_err(),
            "expected parse failure for invalid backend"
        );
        let message = parsed.err().map_or_else(String::new, |e| e.to_string());
        assert!(message.contains("bogus") || message.contains("backend"));
    }

    #[test_log::test]
    fn parses_serve_with_custom_max_turn_requests() {
        let parsed = Cli::try_parse_from([
            "acp-llm-adapter",
            "serve",
            "--backend",
            "deepseek",
            "--max-turn-requests",
            "5",
        ]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Serve {
                    backend: Backend::DeepSeek,
                    ..
                }
            })
        ));

        if let Ok(Cli {
            command: Command::Serve {
                max_turn_requests, ..
            },
        }) = parsed
        {
            assert_eq!(max_turn_requests.get(), 5);
        }
    }

    #[test_log::test]
    fn parses_serve_with_mock_backend() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "serve", "--backend", "mock"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Serve {
                    backend: Backend::Mock,
                    ..
                }
            })
        ));
    }

    #[test_log::test]
    fn parses_serve_with_deepseek_backend_explicitly() {
        let parsed = Cli::try_parse_from(["acp-llm-adapter", "serve", "--backend", "deepseek"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Serve {
                    backend: Backend::DeepSeek,
                    ..
                }
            })
        ));
    }

    #[test_log::test]
    fn parses_dev_with_deepseek_backend_and_custom_prompt() {
        let parsed = Cli::try_parse_from([
            "acp-llm-adapter",
            "dev",
            "--backend",
            "deepseek",
            "--prompt",
            "custom prompt",
        ]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Dev {
                    backend: Backend::DeepSeek,
                    prompt,
                }
            }) if prompt == "custom prompt"
        ));
    }

    #[test_log::test]
    fn init_tracing_initializes_or_reports_already_set() {
        // init_tracing uses try_init so it either succeeds or returns
        // an error if a global subscriber is already registered (e.g. by
        // test-log). Either outcome is valid.
        let result = super::init_tracing(None);
        // The only acceptable error is "already set".
        if let Err(ref error) = result {
            let msg = error.to_string();
            assert!(
                msg.contains("already been set") || msg.contains("default"),
                "unexpected init_tracing error: {msg}"
            );
        }
    }

    #[test]
    fn stop_reason_from_finish_all_branches() {
        use super::stop_reason_from_finish;
        use acp_llm_adapter::llm::FinishReason;

        assert_eq!(
            stop_reason_from_finish(&FinishReason::EndTurn),
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason_from_finish(&FinishReason::ToolCalls),
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason_from_finish(&FinishReason::Other("rate_limit".to_string())),
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason_from_finish(&FinishReason::MaxTokens),
            StopReason::MaxTokens
        );
        assert_eq!(
            stop_reason_from_finish(&FinishReason::Refusal),
            StopReason::Refusal
        );
    }

    #[test]
    fn resource_link_prompt_text_without_description() {
        use super::resource_link_prompt_text;
        use agent_client_protocol::schema::v1::ResourceLink;
        let link = ResourceLink::new("docs_name", "file:///ref.md");
        let rendered = resource_link_prompt_text(&link);
        assert!(rendered.contains("docs_name"));
        assert!(!rendered.contains(" - "));
    }

    #[test]
    fn resource_text_prompt_text_basic() {
        use super::resource_text_prompt_text;
        use agent_client_protocol::schema::v1::TextResourceContents;
        let contents = TextResourceContents::new("body text", "file:///ctx.md");
        let rendered = resource_text_prompt_text(&contents);
        assert!(rendered.contains("[resource]"));
        assert!(rendered.contains("file:///ctx.md"));
        assert!(rendered.contains("body text"));
    }

    #[test_log::test]
    fn eof_guard_cancels_token_when_dropped() {
        let token = CancellationToken::new();
        let guard = EofGuard {
            token: token.clone(),
        };
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(token.is_cancelled());
    }

    #[test_log::test(tokio::test)]
    async fn incoming_stream_eof_cancels_shutdown_token() {
        let token = CancellationToken::new();
        let base = futures_util::stream::iter(vec![
            Ok::<String, std::io::Error>("a".to_string()),
            Ok("b".to_string()),
        ]);
        let mut wrapped = attach_eof_guard(base, token.clone());

        let mut count = 0;
        while let Some(item) = wrapped.next().await {
            assert!(item.is_ok());
            count += 1;
        }
        assert_eq!(count, 2);
        // The guard lives in the stream object; only dropping it cancels.
        assert!(!token.is_cancelled());
        drop(wrapped);
        assert!(token.is_cancelled());
    }

    #[test_log::test]
    fn session_new_response_binds_serve_log() {
        let (sink, writer) = LogSink::channel(
            std::env::temp_dir().join(format!("acp-llm-serve-log-test-{}", Uuid::new_v4())),
            8,
        );
        let connection = sink.connection("connection-test");
        assert!(connection.is_ok());
        let Some(connection) = connection.ok() else {
            return;
        };

        bind_session_from_frame(
            &connection,
            r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"session-test"}}"#,
        );

        assert_eq!(connection.session_id().as_deref(), Some("session-test"));
        drop(connection);
        drop(sink);
        drop(writer);
    }

    #[test_log::test]
    fn tracing_events_route_to_session_or_connection_fallback() {
        let root = std::env::temp_dir().join(format!("acp-trace-layer-test-{}", Uuid::new_v4()));
        let (sink, writer) = LogSink::channel(&root, 16);
        let Ok(connection) = sink.connection("connection-test") else {
            return;
        };
        let layer = SessionLogLayer::new(Some(connection.clone()));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let first_span = tracing::info_span!("session", session_id = "session-first");
            let first_guard = first_span.enter();
            tracing::info!(message = "first-only");
            drop(first_guard);

            let second_span = tracing::info_span!("session", session_id = "session-second");
            let second_guard = second_span.enter();
            tracing::info!(message = "second-only");
            drop(second_guard);

            tracing::info!(message = "fallback-only");
        });

        let Ok(first_path) = sink.session_log_path("session-first") else {
            return;
        };
        let Ok(second_path) = sink.session_log_path("session-second") else {
            return;
        };
        let Ok(connection_path) = sink.connection_log_path("connection-test") else {
            return;
        };

        drop(connection);
        drop(sink);
        writer.run();

        let first_log = std::fs::read_to_string(first_path).unwrap_or_default();
        let second_log = std::fs::read_to_string(second_path).unwrap_or_default();
        let fallback_log = std::fs::read_to_string(connection_path).unwrap_or_default();
        assert!(first_log.contains("first-only"));
        assert!(!first_log.contains("second-only"));
        assert!(second_log.contains("second-only"));
        assert!(!second_log.contains("first-only"));
        assert!(fallback_log.contains("fallback-only"));
        assert!(!fallback_log.contains("first-only"));
        assert!(!fallback_log.contains("second-only"));

        let _ = std::fs::remove_dir_all(root);
    }
}
