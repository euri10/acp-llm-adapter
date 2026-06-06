//! Command-line entrypoint for the `DeepSeek` `ACP` adapter.

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
use std::sync::{Arc, Mutex};
use std::{error::Error, process::ExitCode};

use agent_client_protocol::Stdio;
use agent_client_protocol::schema::{
    AvailableCommand, AvailableCommandInput, ContentBlock, EmbeddedResourceResource, Plan,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionNotification, SessionUpdate, StopReason,
    UnstructuredCommandInput,
};
use clap::{Parser, Subcommand};
use deepseek_acp_adapter::deepseek::FinishReason;
use tracing_subscriber::EnvFilter;

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
    WriteTextFileRequester, serve_with_transport,
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

type AdapterResult<T> = Result<T, Box<dyn Error + Send + Sync + 'static>>;

// Re-export session domain types so other modules can use `crate::*` imports.
pub(crate) use session::{
    AdapterState, DEFAULT_MAX_TURN_REQUESTS, PendingToolCalls, PermissionDecision,
    PermissionPosture, ReasoningEffort, SESSION_CONFIG_MODE_ID, SESSION_CONFIG_MODEL_ID,
    SESSION_CONFIG_REASONING_EFFORT_ID, SessionRecord, SessionStore, default_session_modes,
    initial_model_from_env, request_tool_permission, validate_session_model,
};

const ADAPTER_NAME: &str = env!("CARGO_PKG_NAME");
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the list of available slash commands for the `DeepSeek` adapter.
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

/// Build a `Plan` from a user prompt by splitting it into logical steps.
///
/// If the prompt contains multiple sentences, each becomes a plan entry.
/// Otherwise a single entry captures the entire request.
#[must_use]
fn plan_from_prompt(prompt: &str) -> Plan {
    let entries: Vec<PlanEntry> = if prompt.contains('.') || prompt.contains('\n') {
        prompt
            .split(['.', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                PlanEntry::new(
                    s.to_string(),
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::Pending,
                )
            })
            .collect()
    } else {
        vec![PlanEntry::new(
            prompt.to_string(),
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )]
    };

    Plan::new(entries)
}

#[derive(Debug, Parser)]
#[command(
    name = "deepseek-acp-adapter",
    version,
    about = "ACP stdio adapter for DeepSeek-backed coding sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run the ACP server over standard input and output.
    Serve {
        #[arg(long, value_enum, default_value_t = Backend::Real)]
        backend: Backend,
        /// Maximum tool-call/response cycles per prompt turn (must be ≥ 1).
        #[arg(long, default_value_t = DEFAULT_MAX_TURN_REQUESTS)]
        max_turn_requests: NonZeroUsize,
    },
    #[command(hide = true)]
    Dev {
        #[arg(long, value_enum, default_value_t = Backend::Mock)]
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

fn run() -> AdapterResult<()> {
    init_tracing()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        match Cli::parse().command {
            Command::Serve {
                backend,
                max_turn_requests,
            } => serve(backend, max_turn_requests).await,
            Command::Dev { backend, prompt } => dev(backend, prompt).await,
        }
    })?;

    Ok(())
}

fn init_tracing() -> AdapterResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init()?;
    Ok(())
}

async fn serve(
    backend: Backend,
    max_turn_requests: NonZeroUsize,
) -> Result<(), agent_client_protocol::Error> {
    let llm_client = llm_client_for_backend(backend)?;
    let tool_registry = Arc::new(AdapterToolRegistry);
    let state = Arc::new(Mutex::new(AdapterState::new(initial_model_from_env())));
    serve_with_transport(
        Stdio::new(),
        state,
        llm_client,
        tool_registry,
        max_turn_requests,
    )
    .await
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

fn text_from_prompt(prompt: &[ContentBlock]) -> Result<String, agent_client_protocol::Error> {
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
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("binary resource prompt blocks are not supported"));
                }
                _ => {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("unsupported embedded resource prompt block"));
                }
            },
            _ => {
                return Err(agent_client_protocol::Error::invalid_params().data(
                    "only text, resource link, and text resource prompt blocks are supported",
                ));
            }
        }
    }

    if text.trim().is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("prompt must include non-empty text"));
    }

    Ok(text)
}

fn resource_link_prompt_text(link: &agent_client_protocol::schema::ResourceLink) -> String {
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
    contents: &agent_client_protocol::schema::TextResourceContents,
) -> String {
    let mut rendered = String::new();
    rendered.push_str("[resource] <");
    rendered.push_str(&contents.uri);
    rendered.push_str(">\n");
    rendered.push_str(&contents.text);
    rendered
}

fn session_notification(
    session_id: agent_client_protocol::schema::SessionId,
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
    SessionStore::new(Arc::new(Mutex::new(AdapterState::default())))
}

#[cfg(test)]
mod tests {
    use super::{
        Backend, Cli, Command, DEFAULT_MAX_TURN_REQUESTS, plan_from_prompt, text_from_prompt,
    };
    use crate::acp::validate_session_paths;
    use agent_client_protocol::schema::{
        BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
        ImageContent, NewSessionRequest, PlanEntryPriority, ResourceLink, TextResourceContents,
    };
    use clap::Parser;

    #[test_log::test]
    fn parses_serve_subcommand() {
        let parsed = Cli::try_parse_from(["deepseek-acp-adapter", "serve"]);
        assert!(
            matches!(
                parsed,
                Ok(Cli {
                    command: Command::Serve {
                        backend: Backend::Real,
                        ..
                    }
                })
            ),
            "expected Ok(Cli::Serve {{ backend: Real }}), got {parsed:?}"
        );
        if let Ok(Cli {
            command: Command::Serve {
                max_turn_requests, ..
            },
        }) = parsed
        {
            assert_eq!(max_turn_requests, DEFAULT_MAX_TURN_REQUESTS);
        }
    }

    #[test_log::test]
    fn parses_dev_subcommand() {
        let parsed = Cli::try_parse_from([
            "deepseek-acp-adapter",
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
    fn plan_from_prompt_splits_multiple_sentences() {
        let plan = plan_from_prompt("Do X. Do Y.");
        assert_eq!(plan.entries.len(), 2);
        assert!(
            plan.entries
                .iter()
                .all(|entry| entry.priority == PlanEntryPriority::Medium)
        );
    }

    #[test]
    fn plan_from_prompt_splits_newlines() {
        let plan = plan_from_prompt("alpha\nbeta");
        assert_eq!(plan.entries.len(), 2);
    }

    #[test]
    fn plan_from_prompt_single_sentence_uses_high_priority() {
        let plan = plan_from_prompt("Just one sentence");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].priority, PlanEntryPriority::High);
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
}
