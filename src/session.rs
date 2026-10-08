//! Adapter-owned session data, mode policy, and tool-call accumulation.
//!
//! These values do not depend on editor schema or I/O handles. The session
//! store owns resource lifecycles and persistence; ACP builds wire selectors.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use acp_llm_adapter::llm::MessageRole;
use acp_llm_adapter::llm::{ChatError, ChatMessage, ToolCall as ChatToolCall, ToolCallDelta};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use acp_llm_adapter::error::AdapterError;
/// Default maximum number of tool-call/response cycles per prompt turn.
pub(crate) const DEFAULT_MAX_TURN_REQUESTS: NonZeroUsize = NonZeroUsize::MIN.saturating_add(99);
pub(crate) const PERMISSION_ALLOW_ONCE_OPTION_ID: &str = "allow_once";
pub(crate) const PERMISSION_ALLOW_ALWAYS_OPTION_ID: &str = "allow_always";
pub(crate) const PERMISSION_REJECT_ONCE_OPTION_ID: &str = "reject_once";
pub(crate) const PERMISSION_REJECT_ALWAYS_OPTION_ID: &str = "reject_always";
pub(crate) const SESSION_MODE_ASK_ID: &str = "ask";
pub(crate) const SESSION_MODE_ACCEPT_EDITS_ID: &str = "accept-edits";
pub(crate) const SESSION_MODE_PLAN_ID: &str = "plan";
pub(crate) const SESSION_MODE_YOLO_ID: &str = "yolo";
pub(crate) const SESSION_CONFIG_MODE_ID: &str = "mode";
pub(crate) const SESSION_CONFIG_MODEL_ID: &str = "model";
pub(crate) const SESSION_CONFIG_REASONING_EFFORT_ID: &str = "reasoning_effort";
pub(crate) const SESSION_CONFIG_MAX_TOKENS_ID: &str = "max_tokens";
pub(crate) const REASONING_EFFORT_HIGH_ID: &str = "high";
pub(crate) const REASONING_EFFORT_MAX_ID: &str = "max";
pub(crate) const MAX_TOKENS_DEFAULT_ID: &str = "default";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionDecision {
    AllowOnce,
    AllowAlways,
    AllowByMode,
    RejectOnce,
    RejectAlways,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SessionBehavior {
    #[default]
    Ask,
    AcceptEdits,
    Plan,
    Yolo,
}

impl SessionBehavior {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::AcceptEdits => "Accept edits",
            Self::Plan => "Plan",
            Self::Yolo => "Yolo",
        }
    }

    pub(crate) const fn mode_id(self) -> &'static str {
        match self {
            Self::Ask => SESSION_MODE_ASK_ID,
            Self::AcceptEdits => SESSION_MODE_ACCEPT_EDITS_ID,
            Self::Plan => SESSION_MODE_PLAN_ID,
            Self::Yolo => SESSION_MODE_YOLO_ID,
        }
    }

    pub(crate) fn from_mode_id_str(mode_id: &str) -> Option<Self> {
        match mode_id {
            SESSION_MODE_ASK_ID => Some(Self::Ask),
            SESSION_MODE_ACCEPT_EDITS_ID => Some(Self::AcceptEdits),
            SESSION_MODE_PLAN_ID => Some(Self::Plan),
            SESSION_MODE_YOLO_ID => Some(Self::Yolo),
            _ => None,
        }
    }

    pub(crate) const fn allows_without_prompt(self, kind: ToolKind) -> bool {
        match self {
            Self::Ask | Self::Plan => false,
            Self::AcceptEdits => matches!(kind, ToolKind::Edit),
            Self::Yolo => !matches!(
                kind,
                ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch
            ),
        }
    }

    pub(crate) const fn allows_tool_kind(self, kind: ToolKind) -> bool {
        match self {
            Self::Plan => matches!(
                kind,
                ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch
            ),
            Self::Ask | Self::AcceptEdits | Self::Yolo => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasoningEffort {
    #[default]
    Default,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => REASONING_EFFORT_HIGH_ID,
            Self::Max => REASONING_EFFORT_MAX_ID,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Default => "Provider default",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Max => "Max",
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Default => "Use the provider's default reasoning effort.",
            Self::Low => "Low reasoning effort.",
            Self::Medium => "Medium reasoning effort.",
            Self::High => "High reasoning effort.",
            Self::Max => "Maximum reasoning effort for complex agent work.",
        }
    }

    pub(crate) fn from_value_id(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            REASONING_EFFORT_HIGH_ID => Some(Self::High),
            REASONING_EFFORT_MAX_ID => Some(Self::Max),
            _ => None,
        }
    }

    pub(crate) fn supported_for_model(model: &str) -> &'static [Self] {
        match model {
            // https://console.groq.com/docs/api-reference#chat-create
            "openai/gpt-oss-120b" | "openai/gpt-oss-20b" => {
                &[Self::Default, Self::Low, Self::Medium, Self::High]
            }
            // https://api-docs.deepseek.com/api/create-chat-completion/
            // Legacy Flash IDs remain accepted: https://api-docs.deepseek.com/
            "deepseek-v4-pro"
            | "deepseek-flash"
            | "deepseek-v4-flash"
            | "deepseek-v4-flash-vision-exp" => &[Self::Default, Self::Low, Self::High, Self::Max],
            // GLM-4.6 does not support this parameter (Z.ai documents support
            // starting at GLM-5.2). Unknown model contracts are not guessed.
            // https://docs.z.ai/api-reference/llm/chat-completion
            _ => &[Self::Default],
        }
    }

    pub(crate) fn for_model(self, model: &str) -> Self {
        if Self::supported_for_model(model).contains(&self) {
            self
        } else {
            Self::Default
        }
    }
}

// Local defensive bound, not a provider capability: allow large tool batches
// while limiting slot allocation and synchronous work driven by untrusted indices.
const MAX_TOOL_CALLS_PER_COMPLETION: usize = 128;

#[derive(Debug, Default)]
pub(crate) struct PendingToolCalls {
    calls: Vec<PendingToolCall>,
}

impl PendingToolCalls {
    /// Accumulate a fragment after validating its slot index.
    ///
    /// # Errors
    ///
    /// Returns an invalid-provider-response error without changing pending calls
    /// when the index exceeds the local per-completion bound.
    pub(crate) fn push(&mut self, delta: &ToolCallDelta) -> Result<(), AdapterError> {
        let index = delta.index();
        if index >= MAX_TOOL_CALLS_PER_COMPLETION {
            return Err(ChatError::InvalidResponse(format!(
                "tool call index {index} exceeds the per-completion limit of {MAX_TOOL_CALLS_PER_COMPLETION} calls"
            ))
            .into());
        }
        while self.calls.len() <= index {
            self.calls.push(PendingToolCall::default());
        }

        if let Some(call) = self.calls.get_mut(index) {
            if let Some(id) = delta.id() {
                call.id = Some(id.to_string());
            }
            if let Some(name) = delta.name() {
                call.name = Some(name.to_string());
            }
            if let Some(arguments) = delta.arguments() {
                call.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<ChatToolCall>, AdapterError> {
        self.calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| call.finish(index))
            .collect()
    }
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PendingToolCall {
    fn finish(self, index: usize) -> Result<ChatToolCall, AdapterError> {
        let id = self.id.ok_or_else(|| {
            AdapterError::InvalidParams(format!("tool call delta {index} is missing an id"))
        })?;
        let name = self.name.ok_or_else(|| {
            AdapterError::InvalidParams(format!(
                "tool call delta {index} is missing a function name"
            ))
        })?;

        Ok(ChatToolCall::new(id, name, self.arguments))
    }
}

/// Parse a session-config value id back into a `max_tokens` setting.
///
/// Returns `Ok(None)` for the `default` id (unset the override) and
/// `Ok(Some(tokens))` for a positive integer id. Rejects anything else.
pub(crate) fn max_tokens_from_value_id(value: &str) -> Result<Option<u32>, AdapterError> {
    if value == MAX_TOKENS_DEFAULT_ID {
        return Ok(None);
    }

    match value.parse::<u32>() {
        Ok(tokens) if tokens > 0 => Ok(Some(tokens)),
        _ => Err(AdapterError::InvalidParams(format!(
            "unsupported max_tokens value: {value}"
        ))),
    }
}

pub(crate) fn validate_session_model(
    session: &SessionRecord,
    model: &str,
    available_models: &[String],
) -> Result<(), AdapterError> {
    if is_known_model(model, available_models) || model == session.model {
        return Ok(());
    }

    Err(AdapterError::InvalidParams(format!(
        "unsupported model: {model}"
    )))
}

fn is_known_model(model: &str, available_models: &[String]) -> bool {
    available_models.iter().any(|m| m == model)
}

/// Categories used by permission and planning policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Read,
    Search,
    Edit,
    Execute,
    Think,
    Fetch,
    Other,
}

/// Context shared by tool policy and execution, without editor wire types.
#[derive(Debug, Clone)]
pub(crate) struct ToolContext {
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) client_capabilities: Option<ClientCapabilities>,
}

/// Editor features consumed by built-in tools, independent of the ACP schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ClientCapabilities {
    pub(crate) read_text_file: bool,
    pub(crate) write_text_file: bool,
    pub(crate) terminal: bool,
}

/// Listable session metadata; the ACP edge builds the wire response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) title: Option<String>,
    pub(crate) updated_at: Option<String>,
    pub(crate) meta: Option<serde_json::Map<String, serde_json::Value>>,
}

impl SessionSummary {
    pub(crate) fn new(session_id: String, cwd: PathBuf) -> Self {
        Self {
            session_id,
            cwd,
            additional_directories: Vec::new(),
            title: None,
            updated_at: None,
            meta: None,
        }
    }
    pub(crate) fn additional_directories(mut self, paths: Vec<PathBuf>) -> Self {
        self.additional_directories = paths;
        self
    }
    pub(crate) fn title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }
    pub(crate) fn updated_at(mut self, updated_at: String) -> Self {
        self.updated_at = Some(updated_at);
        self
    }
}

// The calendar arithmetic behind this helper moved into the library crate so
// the log sink — shared with the proxy binary — can stamp records without a
// second copy of it.
pub(crate) use acp_llm_adapter::timestamp::iso_timestamp_now;

/// Derive a human-readable session title from the message history.
///
/// Returns the first user message's content truncated to 80 characters, or
/// `"New session"` if no user message is found.
pub(crate) fn derive_session_title(history: &[ChatMessage]) -> String {
    history
        .iter()
        .find(|msg| msg.role() == MessageRole::User)
        .map_or_else(
            || "New session".to_string(),
            |msg| format_session_title(msg.content()),
        )
}

pub(crate) fn format_session_title(content: &str) -> String {
    const MAX_TITLE_LEN: usize = 80;

    let text: String = content
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_TITLE_LEN {
        trimmed.to_string()
    } else {
        format!(
            "{}…",
            trimmed.chars().take(MAX_TITLE_LEN).collect::<String>()
        )
    }
}

#[derive(Debug)]
pub(crate) struct SessionRecord {
    pub(crate) selected_content: Option<crate::selected_content::Limits>,
    pub(crate) selected_content_used: bool,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) history: Vec<ChatMessage>,
    pub(crate) active_turn: Option<CancellationToken>,
    pub(crate) mode: SessionBehavior,
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffort,
    /// Maximum tokens the LLM may generate per response. `None` means use
    /// the model's own default (the parameter is omitted from the request).
    pub(crate) max_tokens: Option<u32>,
    pub(crate) permission_allow_always: HashSet<String>,
    pub(crate) permission_reject_always: HashSet<String>,
    /// Human-readable session title, derived from the first user message.
    pub(crate) title: String,
    /// ISO 8601 timestamp of the last activity.
    pub(crate) updated_at: String,
    /// Cumulative `DeepSeek` cost in microdollars.
    pub(crate) cost_micros: u64,
}

/// Snapshot of session data needed to begin a prompt turn.
///
/// Returned by [`crate::session_store::SessionStore::begin_turn`] so the caller does not need to
/// hold the lock across model streaming.
#[derive(Debug)]
pub(crate) struct TurnSetup {
    pub(crate) selected_content: Option<crate::selected_content::Limits>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) tool_context: ToolContext,
    pub(crate) behavior: SessionBehavior,
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffort,
    pub(crate) max_tokens: Option<u32>,
    /// Human-readable session title after the turn begins.
    pub(crate) title: String,
    /// Whether this turn derived the title for the first time.
    pub(crate) title_changed: bool,
    /// ISO 8601 timestamp of the session's latest activity.
    pub(crate) updated_at: String,
}

#[cfg(test)]
pub(crate) mod tests;
