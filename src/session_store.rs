//! Filesystem-backed session persistence.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::mcp::McpSession;
use crate::mcp::McpToolTarget;
use crate::session::{
    ClientCapabilities, SessionRecord, SessionSummary, ToolContext, TurnSetup,
    derive_session_title, format_session_title, iso_timestamp_now,
};
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::error::SessionPersistenceError;
use acp_llm_adapter::llm::ChatMessage;
use acp_llm_adapter::llm::ToolDefinition;
use acp_llm_adapter::llm::{ChatConfig, ChatError, ModelCatalog};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::{ReasoningEffort, SessionBehavior};

/// Live I/O handles and reconnect settings owned by the session-store boundary.
#[derive(Debug, Default)]
struct SessionResources {
    servers: Vec<serde_json::Value>,
    sessions: Vec<McpSession>,
}

#[derive(Debug)]
pub(crate) struct AdapterState {
    pub(crate) default_model: String,
    /// Model IDs available for selection. Set at startup via
    /// `ChatClient::fetch_available_models` or initialised to
    /// `[default_model]` as a fallback.
    pub(crate) model_catalog: ModelCatalog,
    pub(crate) client_capabilities: Option<ClientCapabilities>,
    pub(crate) sessions: HashMap<String, SessionRecord>,
    resources: HashMap<String, SessionResources>,
    /// Removed sessions still own their prompt until cancellation cleanup ends.
    retired_turns: HashMap<String, CancellationToken>,
}

impl AdapterState {
    pub(crate) fn new(default_model: impl Into<String>) -> Self {
        let model = default_model.into();
        let available_models = vec![model.clone()];
        Self {
            default_model: model,
            model_catalog: ModelCatalog {
                ids: available_models,
                ..ModelCatalog::default()
            },
            client_capabilities: None,
            sessions: HashMap::new(),
            resources: HashMap::new(),
            retired_turns: HashMap::new(),
        }
    }

    /// Replace the known model list with the result of dynamic discovery.
    ///
    /// The first element must be the current `default_model` so the UI
    /// selector defaults correctly.
    #[allow(dead_code)] // wired in Phase 5 (model fetch at startup)
    pub(crate) fn set_available_models(&mut self, models: Vec<String>) {
        self.model_catalog = ModelCatalog {
            ids: models,
            ..ModelCatalog::default()
        };
    }

    fn remove_session(&mut self, session_id: &str) -> bool {
        let Some(session) = self.sessions.remove(session_id) else {
            return false;
        };
        if let Some(token) = session.active_turn {
            token.cancel();
            self.retired_turns.insert(session_id.to_string(), token);
        }
        self.resources.remove(session_id);
        true
    }
}

impl Default for AdapterState {
    fn default() -> Self {
        Self::new(ChatConfig::DEFAULT_MODEL)
    }
}

/// Narrow boundary around shared adapter state.
///
/// `SessionStore` wraps the internal `Arc<Mutex<AdapterState>>` and exposes
/// targeted methods for session lifecycle, config, permission cache, and
/// active-turn management. Application code uses `SessionStore` directly
/// instead of passing raw `Arc<Mutex<AdapterState>>` through every layer.
#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    pub(crate) state: Arc<Mutex<AdapterState>>,
    pub(crate) persistence: Option<FilesystemSessionStore>,
    pub(crate) logging_enabled: bool,
}

impl SessionStore {
    /// Wrap an existing `Arc<Mutex<AdapterState>>` in a `SessionStore`.
    pub(crate) fn new(state: Arc<Mutex<AdapterState>>) -> Self {
        Self {
            state,
            persistence: None,
            logging_enabled: false,
        }
    }

    /// Attach filesystem persistence to the session store.
    pub(crate) fn with_persistence(mut self, persistence: FilesystemSessionStore) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Mark structured serve logging as available for `_meta` log paths.
    pub(crate) fn with_logging_enabled(mut self, enabled: bool) -> Self {
        self.logging_enabled = enabled;
        self
    }

    /// Store the client capabilities reported during initialization.
    pub(crate) fn record_client_capabilities(
        &self,
        client_capabilities: ClientCapabilities,
    ) -> Result<(), AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        guard.client_capabilities = Some(client_capabilities);
        Ok(())
    }

    /// Remove a session by id. Returns `true` if the session existed.
    pub(crate) fn remove_session(&self, session_id: &str) -> Result<bool, AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        Ok(guard.remove_session(session_id))
    }

    /// Insert a new session record.
    #[cfg(test)]
    pub(crate) fn insert_session(
        &self,
        session_id: String,
        record: SessionRecord,
    ) -> Result<(), AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        guard.resources.remove(&session_id);
        guard.sessions.insert(session_id, record);
        Ok(())
    }

    /// Return the default model identifier for new sessions.
    pub(crate) fn default_model(&self) -> Result<String, AdapterError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        Ok(guard.default_model.clone())
    }

    /// Return the list of model IDs available for selection.
    ///
    /// The list is populated at startup by dynamic model discovery or falls
    /// back to `[default_model]` when discovery is unavailable.
    pub(crate) fn available_models(&self) -> Result<Vec<String>, AdapterError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        Ok(guard.model_catalog.ids.clone())
    }

    /// Install discovery metadata in test stores; production sets it at startup.
    #[cfg(test)]
    pub(crate) fn set_model_catalog(&self, models: ModelCatalog) -> Result<(), AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        guard.model_catalog = models;
        Ok(())
    }

    /// Look up context metadata for the model selected for this request.
    pub(crate) fn model_context_window(&self, model: &str) -> Result<Option<u64>, AdapterError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        Ok(guard.model_catalog.context_window(model))
    }

    /// Look up a session and return a read-only reference via a callback.
    ///
    /// The lock is held only for the duration of the callback.
    pub(crate) fn with_session<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&SessionRecord) -> Result<T, AdapterError>,
    ) -> Result<T, AdapterError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        let session = guard.sessions.get(session_id).ok_or_else(|| {
            AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
        })?;
        f(session)
    }

    /// Look up a session and invoke a callback with a mutable reference.
    ///
    /// The lock is held only for the duration of the callback.
    pub(crate) fn with_session_mut<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut SessionRecord) -> Result<T, AdapterError>,
    ) -> Result<T, AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        let session = guard.sessions.get_mut(session_id).ok_or_else(|| {
            AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
        })?;
        f(session)
    }

    /// Check whether a tool name is in the session's allow-always cache.
    pub(crate) fn is_always_allowed(
        &self,
        session_id: &str,
        tool_name: &str,
    ) -> Result<bool, AdapterError> {
        self.with_session(session_id, |session| {
            Ok(session.permission_allow_always.contains(tool_name))
        })
    }

    /// Check whether the editor rejected a tool for the remainder of this session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unknown or the state lock is poisoned.
    pub(crate) fn is_always_rejected(
        &self,
        session_id: &str,
        tool_name: &str,
    ) -> Result<bool, AdapterError> {
        self.with_session(session_id, |session| {
            Ok(session.permission_reject_always.contains(tool_name))
        })
    }

    /// Return the current session behavior (mode) for a session.
    pub(crate) fn session_behavior(
        &self,
        session_id: &str,
    ) -> Result<SessionBehavior, AdapterError> {
        self.with_session(session_id, |session| Ok(session.mode))
    }

    pub(crate) fn selected_content_limits(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::selected_content::Limits>, AdapterError> {
        self.with_session(session_id, |session| Ok(session.selected_content))
    }

    /// Insert a tool name into the session's allow-always cache.
    pub(crate) fn add_always_allow(
        &self,
        session_id: &str,
        tool_name: String,
    ) -> Result<(), AdapterError> {
        self.with_session_mut(session_id, |session| {
            session.permission_allow_always.insert(tool_name);
            Ok(())
        })
    }

    /// Remember an explicit editor denial independently of the mutable mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unknown or the state lock is poisoned.
    pub(crate) fn add_always_reject(
        &self,
        session_id: &str,
        tool_name: String,
    ) -> Result<(), AdapterError> {
        self.with_session_mut(session_id, |session| {
            session.permission_reject_always.insert(tool_name);
            Ok(())
        })
    }

    /// Cancel the active turn token for a session, if one exists.
    pub(crate) fn cancel_active_turn(&self, session_id: &str) -> Result<(), AdapterError> {
        self.with_session(session_id, |session| {
            if let Some(token) = &session.active_turn {
                token.cancel();
            }
            Ok(())
        })
    }

    /// Release only the completing turn's ownership, including after removal.
    pub(crate) fn clear_active_turn(
        &self,
        session_id: &str,
        token: &CancellationToken,
    ) -> Result<(), AdapterError> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if state.retired_turns.get(session_id) == Some(token) {
            state.retired_turns.remove(session_id);
        }
        if let Some(session) = state.sessions.get_mut(session_id)
            && session.active_turn.as_ref() == Some(token)
        {
            session.active_turn = None;
        }
        Ok(())
    }

    /// Persist the session behavior before publishing the change.
    /// Production callers run settings updates on the blocking pool.
    pub(crate) fn set_mode(
        &self,
        session_id: &str,
        mode: SessionBehavior,
    ) -> Result<(), AdapterError> {
        self.update_settings(session_id, |session| {
            session.mode = mode;
            Ok(())
        })
    }

    /// Set the model for a session.
    pub(crate) fn set_model(&self, session_id: &str, model: String) -> Result<(), AdapterError> {
        self.update_settings(session_id, |session| {
            session.reasoning_effort = session.reasoning_effort.for_model(&model);
            session.model = model;
            Ok(())
        })
    }

    /// Set the reasoning effort for a session.
    pub(crate) fn set_reasoning_effort(
        &self,
        session_id: &str,
        effort: ReasoningEffort,
    ) -> Result<(), AdapterError> {
        self.update_settings(session_id, |session| {
            if !ReasoningEffort::supported_for_model(&session.model).contains(&effort) {
                return Err(AdapterError::InvalidParams(
                    "unsupported reasoning effort for selected model".into(),
                ));
            }
            session.reasoning_effort = effort;
            Ok(())
        })
    }

    /// Set the max output token cap for a session. `None` unsets the override.
    pub(crate) fn set_max_tokens(
        &self,
        session_id: &str,
        max_tokens: Option<u32>,
    ) -> Result<(), AdapterError> {
        self.update_settings(session_id, |session| {
            session.max_tokens = max_tokens;
            Ok(())
        })
    }

    /// Serialize settings with history saves and removal, publishing only after
    /// the atomic metadata write succeeds. The synchronous transaction runs on
    /// the blocking pool in production, like `save_history`.
    fn update_settings(
        &self,
        session_id: &str,
        update: impl FnOnce(&mut PersistedSessionMeta) -> Result<(), AdapterError>,
    ) -> Result<(), AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|error| AdapterError::Internal(error.to_string()))?;
        let state = &mut *guard;
        let session = state.sessions.get_mut(session_id).ok_or_else(|| {
            AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
        })?;
        let mut meta = session.persisted_meta(
            session_id,
            state
                .resources
                .get(session_id)
                .map_or(&[], |resources| resources.servers.as_slice()),
        );
        update(&mut meta)?;
        if session.selected_content.is_none()
            && let Some(persistence) = &self.persistence
        {
            persistence.persist_turn(&meta, &[])?;
        }
        session.mode = meta.mode;
        session.model = meta.model;
        session.reasoning_effort = meta.reasoning_effort;
        session.max_tokens = meta.max_tokens;
        Ok(())
    }

    /// Prepare a session for a new prompt turn.
    ///
    /// Sets the active turn token atomically and returns the messages, tool
    /// context, model, and reasoning effort the caller needs. Returns an error
    /// if a turn is already active.
    pub(crate) fn begin_turn(
        &self,
        session_id: &str,
        token: CancellationToken,
        user_message: ChatMessage,
        prompt_title: Option<&str>,
    ) -> Result<TurnSetup, AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        let client_capabilities = guard.client_capabilities;
        let session = guard.sessions.get_mut(session_id).ok_or_else(|| {
            AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
        })?;

        if session.active_turn.is_some() {
            return Err(AdapterError::InvalidRequest(format!(
                "session {session_id} already has an active turn"
            )));
        }
        if let Some(limits) = session.selected_content {
            if session.selected_content_used {
                return Err(AdapterError::InvalidRequest(
                    "selected-content Sessions allow one attempt".into(),
                ));
            }
            if user_message.content().len() > limits.input_bytes {
                return Err(AdapterError::InvalidParams(
                    "selected-content input byte limit exceeded".into(),
                ));
            }
            session.selected_content_used = true;
        }
        session.active_turn = Some(token);

        // Bump the last-activity timestamp on every prompt turn.
        session.updated_at = iso_timestamp_now();

        // Derive the title once from ACP text-block structure when available,
        // falling back to the flattened message when no text block is present.
        let title_changed = session.title.is_empty() && session.selected_content.is_none();
        if title_changed {
            session.title = prompt_title.map_or_else(
                || {
                    let mut candidate_messages = session.history.clone();
                    candidate_messages.push(user_message.clone());
                    derive_session_title(&candidate_messages)
                },
                format_session_title,
            );
        }

        let mut messages = session.history.clone();
        messages.push(user_message);
        Ok(TurnSetup {
            selected_content: session.selected_content,
            messages,
            tool_context: ToolContext {
                session_id: session_id.to_string(),
                cwd: session.cwd.clone(),
                additional_directories: session.additional_directories.clone(),
                client_capabilities,
            },
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort,
            max_tokens: session.max_tokens,
            title: session.title.clone(),
            title_changed,
            updated_at: session.updated_at.clone(),
        })
    }

    /// Add a model cost to a session and return its cumulative cost.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unavailable or its cumulative cost
    /// overflows. On overflow, the existing cost is unchanged.
    pub(crate) fn add_cost_micros(
        &self,
        session_id: &str,
        cost_micros: u64,
    ) -> Result<u64, AdapterError> {
        self.with_session_mut(session_id, |session| {
            session.cost_micros =
                session
                    .cost_micros
                    .checked_add(cost_micros)
                    .ok_or_else(|| {
                        ChatError::InvalidResponse(
                            "cumulative session cost exceeds the supported range".to_string(),
                        )
                    })?;
            Ok(session.cost_micros)
        })
    }

    /// Look up a session record for a new-session response.
    pub(crate) fn lookup_session(&self, session_id: &str) -> Result<(), AdapterError> {
        self.with_session(session_id, |_session| Ok(()))
    }
}

const SESSIONS_DIR: &str = "sessions";
const META_FILE: &str = "meta.json";
const HISTORY_FILE: &str = "history.jsonl";

/// Filesystem-backed persistence for ACP session metadata and chat history.
#[derive(Debug, Clone)]
pub(crate) struct FilesystemSessionStore {
    state_dir: PathBuf,
}

/// Persisted metadata for one ACP session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedSessionMeta {
    /// ACP session id.
    pub(crate) session_id: String,
    /// Session working directory.
    pub(crate) cwd: PathBuf,
    /// Additional directories available to the session.
    pub(crate) additional_directories: Vec<PathBuf>,
    /// Session mode active for the session.
    pub(crate) mode: SessionBehavior,
    /// Model selected for the session.
    pub(crate) model: String,
    /// LLM reasoning effort selected for the session.
    pub(crate) reasoning_effort: ReasoningEffort,
    /// Max output token cap selected for the session (absent in sessions
    /// created before this field was added, treated as unset).
    pub(crate) max_tokens: Option<u32>,
    /// MCP servers originally attached to the session.
    pub(crate) mcp_servers: Vec<serde_json::Value>,
    /// Human-readable session title (absent in sessions created before this field was added).
    pub(crate) title: Option<String>,
    /// ISO 8601 timestamp of last activity (absent in sessions created before this field was added).
    pub(crate) updated_at: Option<String>,
    /// Cumulative `DeepSeek` cost in microdollars.
    #[serde(default)]
    pub(crate) cost_micros: u64,
}

/// Persisted session metadata plus replayable chat history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedSessionRecord {
    /// Metadata loaded from `meta.json`.
    pub(crate) meta: PersistedSessionMeta,
    /// Chat messages loaded from `history.jsonl`.
    pub(crate) history: Vec<ChatMessage>,
}

impl FilesystemSessionStore {
    /// Create a store rooted at `state_dir`.
    pub(crate) fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    /// Create a store rooted under `XDG_STATE_HOME` or `$HOME/.local/state`.
    pub(crate) fn from_default_state_dir() -> Result<Self, SessionPersistenceError> {
        Ok(Self::new(default_state_dir()?))
    }

    /// Append a completed turn's new messages and refresh session metadata.
    pub(crate) fn persist_turn(
        &self,
        meta: &PersistedSessionMeta,
        messages: &[ChatMessage],
    ) -> Result<(), SessionPersistenceError> {
        let session_dir = self.session_dir(&meta.session_id)?;
        fs::create_dir_all(&session_dir)?;
        Self::write_meta(&session_dir, meta)?;
        Self::append_history(&session_dir, messages)?;
        Ok(())
    }

    /// Replace history atomically, retaining current metadata and cumulative spend.
    pub(crate) fn clear_history(
        &self,
        meta: &PersistedSessionMeta,
    ) -> Result<(), SessionPersistenceError> {
        let session_dir = self.session_dir(&meta.session_id)?;
        fs::create_dir_all(&session_dir)?;
        let temporary_history = session_dir.join("history.jsonl.tmp");
        File::create(&temporary_history)?.sync_all()?;
        // Settings may have changed since the last prompt. A failed metadata
        // write must not destroy the existing conversation.
        Self::write_meta(&session_dir, meta)?;
        fs::rename(temporary_history, session_dir.join(HISTORY_FILE))?;
        Ok(())
    }

    /// Load one persisted session record by id.
    pub(crate) fn load_record(
        &self,
        session_id: &str,
    ) -> Result<PersistedSessionRecord, SessionPersistenceError> {
        let session_dir = self.session_dir(session_id)?;
        let meta = Self::read_meta(&session_dir)?;
        let history = Self::read_history(&session_dir)?;
        Ok(PersistedSessionRecord { meta, history })
    }

    /// Delete a persisted session directory, including metadata and history.
    ///
    /// Returns `true` when a session directory existed and was removed.
    pub(crate) fn delete_session(&self, session_id: &str) -> Result<bool, SessionPersistenceError> {
        let session_dir = self.session_dir(session_id)?;
        if !session_dir.exists() {
            return Ok(false);
        }

        fs::remove_dir_all(session_dir)?;
        Ok(true)
    }

    /// List persisted sessions, optionally filtered by working directory.
    pub(crate) fn list_persisted(
        &self,
        cwd_filter: Option<&Path>,
    ) -> Result<Vec<SessionSummary>, SessionPersistenceError> {
        let sessions_dir = self.state_dir.join(SESSIONS_DIR);
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(sessions_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(meta) = Self::read_meta(&entry.path()) {
                if cwd_filter.is_some_and(|cwd| meta.cwd != cwd) {
                    continue;
                }
                let mut info = SessionSummary::new(meta.session_id, meta.cwd)
                    .additional_directories(meta.additional_directories);
                if let Some(title) = &meta.title {
                    info = info.title(title.clone());
                }
                if let Some(updated_at) = &meta.updated_at {
                    info = info.updated_at(updated_at.clone());
                }
                sessions.push(info);
            }
        }
        Ok(sessions)
    }

    /// Return the absolute path to the session's `history.jsonl` file.
    ///
    /// # Errors
    ///
    /// Returns [`SessionPersistenceError::InvalidSessionId`] if the session id
    /// contains path separators or other invalid characters.
    pub(crate) fn history_jsonl_path(
        &self,
        session_id: &str,
    ) -> Result<PathBuf, SessionPersistenceError> {
        Ok(self.session_dir(session_id)?.join(HISTORY_FILE))
    }

    /// Return the absolute path to the session's structured log file.
    pub(crate) fn log_jsonl_path(
        &self,
        session_id: &str,
    ) -> Result<PathBuf, SessionPersistenceError> {
        Ok(self.session_dir(session_id)?.join("log.jsonl"))
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, SessionPersistenceError> {
        validate_session_id(session_id)?;
        Ok(self.state_dir.join(SESSIONS_DIR).join(session_id))
    }

    fn write_meta(
        session_dir: &Path,
        meta: &PersistedSessionMeta,
    ) -> Result<(), SessionPersistenceError> {
        let tmp_path = session_dir.join("meta.json.tmp");
        let mut file = File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, meta)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(tmp_path, session_dir.join(META_FILE))?;
        Ok(())
    }

    fn append_history(
        session_dir: &Path,
        messages: &[ChatMessage],
    ) -> Result<(), SessionPersistenceError> {
        if messages.is_empty() {
            return Ok(());
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_dir.join(HISTORY_FILE))?;
        for message in messages {
            serde_json::to_writer(&mut file, message)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        Ok(())
    }

    fn read_meta(session_dir: &Path) -> Result<PersistedSessionMeta, SessionPersistenceError> {
        let file = File::open(session_dir.join(META_FILE))?;
        Ok(serde_json::from_reader(file)?)
    }

    fn read_history(session_dir: &Path) -> Result<Vec<ChatMessage>, SessionPersistenceError> {
        let path = session_dir.join(HISTORY_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            messages.push(serde_json::from_str(&line)?);
        }
        Ok(messages)
    }
}

/// Resolve the state directory, raising this module's error on failure.
///
/// The resolution rules live in the library so the proxy binary shares them.
fn default_state_dir() -> Result<PathBuf, SessionPersistenceError> {
    acp_llm_adapter::paths::default_state_dir().ok_or_else(|| {
        SessionPersistenceError::StateDir("neither XDG_STATE_HOME nor HOME is set".to_string())
    })
}

fn validate_session_id(session_id: &str) -> Result<(), SessionPersistenceError> {
    let valid = !session_id.is_empty()
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));

    if valid {
        Ok(())
    } else {
        Err(SessionPersistenceError::InvalidSessionId(
            session_id.to_string(),
        ))
    }
}

impl SessionRecord {
    fn persisted_meta(
        &self,
        session_id: &str,
        servers: &[serde_json::Value],
    ) -> PersistedSessionMeta {
        PersistedSessionMeta {
            session_id: session_id.to_string(),
            cwd: self.cwd.clone(),
            additional_directories: self.additional_directories.clone(),
            mode: self.mode,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            max_tokens: self.max_tokens,
            mcp_servers: servers.to_vec(),
            title: Some(self.title.clone()),
            updated_at: Some(self.updated_at.clone()),
            cost_micros: self.cost_micros,
        }
    }
}

impl SessionStore {
    /// Load a persisted session record from the filesystem store.
    pub(crate) fn load_persisted_record(
        &self,
        session_id: &str,
    ) -> Result<PersistedSessionRecord, AdapterError> {
        let Some(persistence) = &self.persistence else {
            return Err(AdapterError::InvalidRequest(
                "session/load requires filesystem persistence".to_string(),
            ));
        };

        persistence
            .load_record(session_id)
            .map_err(|e| AdapterError::Internal(e.to_string()))
    }

    /// Return the absolute path to the session's `history.jsonl` file, if
    /// filesystem persistence is configured.
    pub(crate) fn history_jsonl_path(&self, session_id: &str) -> Option<PathBuf> {
        self.persistence
            .as_ref()
            .and_then(|p| p.history_jsonl_path(session_id).ok())
    }

    /// Build available history/log metadata, omitting history for ephemeral helpers.
    ///
    /// # Errors
    ///
    /// Returns an error if the session-state lock is poisoned.
    pub(crate) fn session_meta(
        &self,
        session_id: &str,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, AdapterError> {
        let selected_content = self
            .state
            .lock()
            .map_err(|error| AdapterError::Internal(error.to_string()))?
            .sessions
            .get(session_id)
            .is_some_and(|session| session.selected_content.is_some());
        Ok(self.session_meta_for(session_id, selected_content))
    }

    /// Build paths from a session snapshot without reentering the state lock.
    fn session_meta_for(
        &self,
        session_id: &str,
        selected_content: bool,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let path = self.history_jsonl_path(session_id)?;
        let mut meta = serde_json::Map::new();
        if !selected_content {
            meta.insert(
                "historyJsonlPath".to_string(),
                serde_json::Value::String(path.to_string_lossy().to_string()),
            );
        }
        if self.logging_enabled
            && let Some(log_path) = self
                .persistence
                .as_ref()
                .and_then(|p| p.log_jsonl_path(session_id).ok())
        {
            meta.insert(
                "logJsonlPath".to_string(),
                serde_json::Value::String(log_path.to_string_lossy().to_string()),
            );
        }
        (!meta.is_empty()).then_some(meta)
    }

    /// Return a snapshot of matching sessions for the `session/list` handler.
    pub(crate) fn list_sessions(
        &self,
        cwd_filter: Option<&Path>,
    ) -> Result<Vec<SessionSummary>, AdapterError> {
        let (mut sessions, persistence) = {
            let guard = self
                .state
                .lock()
                .map_err(|e| AdapterError::Internal(e.to_string()))?;
            (
                guard
                    .sessions
                    .iter()
                    .filter(|(_session_id, record)| cwd_filter.is_none_or(|cwd| record.cwd == cwd))
                    .map(|(session_id, record)| {
                        let mut info = SessionSummary::new(session_id.clone(), record.cwd.clone())
                            .additional_directories(record.additional_directories.clone());
                        if !record.title.is_empty() {
                            info = info.title(record.title.clone());
                        }
                        info = info.updated_at(record.updated_at.clone());
                        info.meta =
                            self.session_meta_for(session_id, record.selected_content.is_some());
                        info
                    })
                    .collect::<Vec<_>>(),
                self.persistence.clone(),
            )
        };

        if let Some(persistence) = persistence {
            let persisted_list = persistence
                .list_persisted(cwd_filter)
                .map_err(|e| AdapterError::Internal(e.to_string()))?;
            for mut persisted in persisted_list {
                if !sessions
                    .iter()
                    .any(|session| session.session_id == persisted.session_id)
                {
                    persisted.meta = self.session_meta_for(&persisted.session_id, false);
                    sessions.push(persisted);
                }
            }
        }

        // Sort most-recently-updated first so that the /resume picker surfaces
        // recent sessions at the top regardless of filesystem iteration order.
        // Sessions without an `updated_at` timestamp sort to the end.
        sessions.sort_by(|a, b| {
            let a_ts = a.updated_at.as_deref().unwrap_or("");
            let b_ts = b.updated_at.as_deref().unwrap_or("");
            b_ts.cmp(a_ts)
        });

        Ok(sessions)
    }

    /// Remove a session from memory and persistent storage.
    pub(crate) fn delete_session(&self, session_id: &str) -> Result<bool, AdapterError> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        let deleted_from_memory = state.remove_session(session_id);
        // Like history commits, deletion runs on the blocking pool. Serialize
        // both disk and memory with save_history so a pending save cannot
        // resurrect a deleted session after this method returns.
        let deleted_from_persistence = if let Some(persistence) = &self.persistence {
            persistence
                .delete_session(session_id)
                .map_err(|e| AdapterError::Internal(e.to_string()))?
        } else {
            false
        };

        Ok(deleted_from_memory || deleted_from_persistence)
    }

    /// Publish a session and its external resources together.
    ///
    /// Reject replacement until an active turn has finished cleanup, including
    /// after cancellation. This check shares the admission lock so asynchronous
    /// restore setup cannot discard a turn that started while it was awaiting I/O.
    pub(crate) fn insert_session_with_resources(
        &self,
        session_id: String,
        record: SessionRecord,
        servers: Vec<serde_json::Value>,
        sessions: Vec<McpSession>,
    ) -> Result<(), AdapterError> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if state.retired_turns.contains_key(&session_id)
            || state
                .sessions
                .get(&session_id)
                .is_some_and(|session| session.active_turn.is_some())
        {
            return Err(AdapterError::InvalidRequest(
                "cannot restore a session with an active turn".into(),
            ));
        }
        state
            .resources
            .insert(session_id.clone(), SessionResources { servers, sessions });
        state.sessions.insert(session_id, record);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_mcp_session(
        &self,
        session_id: &str,
        session: McpSession,
    ) -> Result<(), AdapterError> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if !state.sessions.contains_key(session_id) {
            return Err(AdapterError::SessionNotFound(session_id.to_string()));
        }
        state
            .resources
            .entry(session_id.to_string())
            .or_default()
            .sessions
            .push(session);
        Ok(())
    }

    /// Return definitions without exposing live MCP peers to the turn core.
    pub(crate) fn mcp_definitions(
        &self,
        session_id: &str,
    ) -> Result<Vec<ToolDefinition>, AdapterError> {
        let state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if !state.sessions.contains_key(session_id) {
            return Err(AdapterError::InvalidParams(format!(
                "unknown session id: {session_id}"
            )));
        }
        Ok(state
            .resources
            .get(session_id)
            .into_iter()
            .flat_map(|resources| &resources.sessions)
            .flat_map(|session| session.tools.iter().map(|tool| tool.definition.clone()))
            .collect())
    }

    /// Resolve a live tool target only at the I/O edge.
    pub(crate) fn find_mcp_target(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<McpToolTarget>, AdapterError> {
        let state = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if !state.sessions.contains_key(session_id) {
            return Err(AdapterError::InvalidParams(format!(
                "unknown session id: {session_id}"
            )));
        }
        Ok(state.resources.get(session_id).and_then(|resources| {
            resources.sessions.iter().find_map(|session| {
                session.tools.iter().find_map(|tool| {
                    (tool.exposed_name == name).then(|| McpToolTarget {
                        server_name: session.name.clone(),
                        original_name: tool.original_name.clone(),
                        peer: session.peer.clone(),
                    })
                })
            })
        }))
    }

    /// Persist the messages as the session history after a turn completes.
    pub(crate) fn save_history(
        &self,
        session_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), AdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?;
        if guard.retired_turns.contains_key(session_id) {
            // A removed turn may finish cancellation bookkeeping, but must not
            // recreate persisted history or update a replacement session.
            return Ok(());
        }
        let state = &mut *guard;
        let session = state.sessions.get_mut(session_id).ok_or_else(|| {
            AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
        })?;
        if session.selected_content.is_some() {
            // Helpers are ephemeral: no raw question, selected source or answer on disk.
            return Ok(());
        }
        let new_messages = messages
            .iter()
            .skip(session.history.len())
            .cloned()
            .collect::<Vec<_>>();
        let meta = session.persisted_meta(
            session_id,
            state
                .resources
                .get(session_id)
                .map(|resources| resources.servers.as_slice())
                .unwrap_or_default(),
        );
        // This synchronous method runs on the blocking pool in production.
        // Hold admission/removal ownership through disk and memory publication.
        if let Some(persistence) = &self.persistence {
            persistence
                .persist_turn(&meta, &new_messages)
                .map_err(|e| AdapterError::Internal(e.to_string()))?;
        }

        session.history = messages.to_vec();
        Ok(())
    }

    /// Clear an idle ordinary session's history, preserving settings and spend.
    ///
    /// Disk replacement precedes the in-memory mutation. The whole transaction
    /// runs on the blocking pool, serialized with session admission/removal; no
    /// lock is held across an await and failures leave conversation history intact.
    pub(crate) async fn clear_history(&self, session_id: &str) -> Result<(), AdapterError> {
        let store = self.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut state = store
                .state
                .lock()
                .map_err(|e| AdapterError::Internal(e.to_string()))?;
            let servers = state
                .resources
                .get(&session_id)
                .map_or_else(Vec::new, |resources| resources.servers.clone());
            let session = state.sessions.get_mut(&session_id).ok_or_else(|| {
                AdapterError::InvalidParams(format!("unknown session id: {session_id}"))
            })?;
            if session.active_turn.is_some() {
                return Err(AdapterError::InvalidRequest(
                    "cannot clear an active turn".into(),
                ));
            }
            if session.selected_content.is_some() {
                return Err(AdapterError::InvalidRequest(
                    "selected-content sessions cannot clear history".into(),
                ));
            }
            if let Some(persistence) = &store.persistence {
                persistence.clear_history(&session.persisted_meta(&session_id, &servers))?;
            }
            session.history.clear();
            Ok(())
        })
        .await
        .map_err(|error| AdapterError::Internal(error.to_string()))?
    }

    /// Persist a turn through the blocking I/O boundary.
    pub(crate) async fn persist_history(
        &self,
        session_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), AdapterError> {
        let store = self.clone();
        let session_id = session_id.to_string();
        let messages = messages.to_vec();
        blocking::unblock(move || store.save_history(&session_id, &messages)).await
    }
}

#[cfg(test)]
mod tests;
