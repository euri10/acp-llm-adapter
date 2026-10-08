//! ACP session modes and config selectors translated from core settings.

use crate::SessionStore;
use crate::session::{
    MAX_TOKENS_DEFAULT_ID, ReasoningEffort, SESSION_CONFIG_MAX_TOKENS_ID, SESSION_CONFIG_MODE_ID,
    SESSION_CONFIG_MODEL_ID, SESSION_CONFIG_REASONING_EFFORT_ID, SessionBehavior, SessionRecord,
};
use acp_llm_adapter::error::AdapterError;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionMode, SessionModeState,
};

/// Preset `max_tokens` values offered in the session config selector.
///
/// The provider's chat-completions API accepts any positive integer for
/// `max_tokens` (bounded by the model's max output), but ACP session config
/// options only support fixed selectable values, not freeform text input.
/// These presets cover common output-length budgets.
const MAX_TOKENS_PRESETS: [u32; 6] = [4_096, 8_192, 16_384, 32_768, 65_536, 131_072];

pub(crate) fn session_modes(current_mode: SessionBehavior) -> SessionModeState {
    SessionModeState::new(
        current_mode.mode_id(),
        vec![
            SessionMode::new(SessionBehavior::Ask.mode_id(), SessionBehavior::Ask.name()),
            SessionMode::new(
                SessionBehavior::AcceptEdits.mode_id(),
                SessionBehavior::AcceptEdits.name(),
            ),
            SessionMode::new(
                SessionBehavior::Plan.mode_id(),
                SessionBehavior::Plan.name(),
            ),
            SessionMode::new(
                SessionBehavior::Yolo.mode_id(),
                SessionBehavior::Yolo.name(),
            ),
        ],
    )
}

pub(crate) fn default_session_modes() -> SessionModeState {
    session_modes(SessionBehavior::Ask)
}

pub(crate) fn session_config_options(
    session: &SessionRecord,
    available_models: &[String],
) -> Vec<SessionConfigOption> {
    vec![
        SessionConfigOption::select(
            SESSION_CONFIG_MODE_ID,
            "Session Mode",
            session.mode.mode_id(),
            session_mode_select_options(),
        )
        .category(SessionConfigOptionCategory::Mode)
        .description("Choose how the adapter behaves for tools and planning."),
        SessionConfigOption::select(
            SESSION_CONFIG_MODEL_ID,
            "Model",
            session.model.clone(),
            model_select_options(session.model.as_str(), available_models),
        )
        .category(SessionConfigOptionCategory::Model)
        .description("Choose which model the adapter should use."),
        SessionConfigOption::select(
            SESSION_CONFIG_REASONING_EFFORT_ID,
            "Reasoning Effort",
            session.reasoning_effort.id(),
            reasoning_effort_select_options(&session.model),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description("Choose how much thinking effort to request."),
        SessionConfigOption::select(
            SESSION_CONFIG_MAX_TOKENS_ID,
            "Max Output Tokens",
            max_tokens_value_id(session.max_tokens),
            max_tokens_select_options(),
        )
        .description("Cap the number of tokens the model may generate in a response."),
    ]
}

fn session_mode_select_options() -> Vec<SessionConfigSelectOption> {
    session_modes(SessionBehavior::Ask)
        .available_modes
        .into_iter()
        .map(|mode| SessionConfigSelectOption::new(mode.id.0, mode.name))
        .collect()
}

/// Build the selectable model list from the dynamically-discovered model IDs.
///
/// If `current_model` is not already in the known list, it is prepended so the
/// current selection is always displayed.
pub(crate) fn model_select_options(
    current_model: &str,
    available_models: &[String],
) -> Vec<SessionConfigSelectOption> {
    let mut options: Vec<SessionConfigSelectOption> = Vec::new();

    if !available_models.iter().any(|model| model == current_model) {
        options.push(
            SessionConfigSelectOption::new(current_model.to_string(), current_model.to_string())
                .description("Current model from LLM_MODEL."),
        );
    }

    for model_id in available_models {
        // Avoid duplicating the custom entry we may have prepended.
        if model_id == current_model && options.iter().any(|o| o.value.0.as_ref() == model_id) {
            continue;
        }
        options.push(
            SessionConfigSelectOption::new(model_id.clone(), model_id.clone())
                .description("Available model."),
        );
    }

    options
}

fn reasoning_effort_select_options(model: &str) -> Vec<SessionConfigSelectOption> {
    ReasoningEffort::supported_for_model(model)
        .iter()
        .map(|effort| {
            SessionConfigSelectOption::new(effort.id(), effort.name())
                .description(effort.description())
        })
        .collect()
}

/// Selectable `max_tokens` values, plus a `default` entry meaning "unset -
/// let the provider use its own default output length".
pub(crate) fn max_tokens_select_options() -> Vec<SessionConfigSelectOption> {
    let mut options = vec![
        SessionConfigSelectOption::new(MAX_TOKENS_DEFAULT_ID, "Default")
            .description("Use the provider's own default output length."),
    ];
    options.extend(
        MAX_TOKENS_PRESETS
            .into_iter()
            .map(|tokens| SessionConfigSelectOption::new(tokens.to_string(), format!("{tokens}"))),
    );
    options
}

/// Map a session's `max_tokens` setting to its session-config value id.
pub(crate) fn max_tokens_value_id(max_tokens: Option<u32>) -> String {
    max_tokens.map_or_else(
        || MAX_TOKENS_DEFAULT_ID.to_string(),
        |tokens| tokens.to_string(),
    )
}

impl SessionStore {
    /// Return the session config options for a session.
    pub(crate) fn session_config_options(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionConfigOption>, AdapterError> {
        let available_models = self.available_models()?;
        self.with_session(session_id, |session| {
            Ok(session_config_options(session, &available_models))
        })
    }
}

impl From<ClientCapabilities> for crate::session::ClientCapabilities {
    fn from(value: ClientCapabilities) -> Self {
        Self {
            read_text_file: value.fs.read_text_file,
            write_text_file: value.fs.write_text_file,
            terminal: value.terminal,
        }
    }
}

use agent_client_protocol::schema::v1::NewSessionRequest;

impl crate::selected_content::Limits {
    pub(crate) fn from_request(
        request: &NewSessionRequest,
    ) -> Result<Option<Self>, agent_client_protocol::Error> {
        let Some(value) = request
            .meta
            .as_ref()
            .and_then(|meta| meta.get(crate::selected_content::META_KEY))
        else {
            return Ok(None);
        };
        let invalid = || {
            agent_client_protocol::Error::invalid_params()
                .data("invalid selected-content creation contract")
        };
        let limits: Self = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        if limits.version != 1
            || !(1..=131_072).contains(&limits.input_bytes)
            || !(1..=65_536).contains(&limits.output_bytes)
            || !(1..=8192).contains(&limits.max_tokens)
            || !(1..=30_000).contains(&limits.timeout_ms)
            || !request.mcp_servers.is_empty()
            || !request.additional_directories.is_empty()
        {
            return Err(invalid());
        }
        Ok(Some(limits))
    }
}
