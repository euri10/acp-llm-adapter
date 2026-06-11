//! Unified error type for the `DeepSeek` ACP adapter.
//!
//! `AdapterError` wraps all domain-level errors produced by the adapter so
//! that callers (primarily the ACP protocol boundary) can convert a single
//! error type into [`agent_client_protocol::Error`] without matching on every
//! internal error variant.

use thiserror::Error;

use crate::deepseek::DeepSeekError;

// ---------------------------------------------------------------------------
// SessionPersistenceError — moved here from the library crate so AdapterError
// can use `#[from]` without crate-boundary headaches.
// ---------------------------------------------------------------------------

/// Error returned by filesystem session persistence.
#[derive(Debug, Error)]
pub enum SessionPersistenceError {
    /// The host environment does not expose a usable state directory.
    #[error("failed to resolve state directory: {0}")]
    StateDir(String),
    /// The session id cannot be represented as a safe path component.
    #[error("invalid persisted session id: {0}")]
    InvalidSessionId(String),
    /// Filesystem I/O failed.
    #[error("filesystem session store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("filesystem session store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// AdapterError — single domain error for the whole adapter
// ---------------------------------------------------------------------------

/// Unified domain error for the `DeepSeek` ACP adapter.
///
/// Every public fallible function in the domain layer returns this type (or a
/// `Result` whose error variant is this type).  The ACP boundary owns the
/// single `From` implementation that converts `AdapterError` into
/// [`agent_client_protocol::Error`].
///
/// # Errors
///
/// Each variant corresponds to a specific failure domain:
///
/// | Variant | Source | Typical cause |
/// |---|---|---|
/// | `DeepSeek` | [`DeepSeekError`] | API key missing, transport failure, bad response |
/// | `SessionPersistence` | [`SessionPersistenceError`] | I/O, JSON, invalid session id |
/// | `InvalidParams` | — | Invalid method parameters |
/// | `InvalidRequest` | — | Invalid request structure |
/// | `SessionNotFound` | — | Session id not found in store |
/// | `Internal` | — | Unexpected internal invariant violations |
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The `DeepSeek` API client returned an error.
    #[error("DeepSeek API error: {0}")]
    DeepSeek(#[from] DeepSeekError),

    /// Session persistence (filesystem I/O, JSON) failed.
    #[error("session persistence error: {0}")]
    SessionPersistence(#[from] SessionPersistenceError),

    /// Invalid method parameters.
    #[error("invalid params: {0}")]
    InvalidParams(String),

    /// Invalid request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Session id not found in the store.
    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// An unexpected internal invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<AdapterError> for agent_client_protocol::Error {
    /// Converts any [`AdapterError`] into an ACP error with the appropriate
    /// JSON-RPC error code.
    fn from(err: AdapterError) -> Self {
        match err {
            AdapterError::InvalidParams(msg) => {
                agent_client_protocol::Error::invalid_params().data(msg)
            }
            AdapterError::InvalidRequest(msg) => {
                agent_client_protocol::Error::invalid_request().data(msg)
            }
            AdapterError::SessionNotFound(id) => agent_client_protocol::Error::invalid_params()
                .data(format!("session not found: {id}")),
            other => agent_client_protocol::Error::into_internal_error(other),
        }
    }
}

impl From<std::io::Error> for AdapterError {
    fn from(err: std::io::Error) -> Self {
        Self::SessionPersistence(SessionPersistenceError::Io(err))
    }
}

impl From<serde_json::Error> for AdapterError {
    fn from(err: serde_json::Error) -> Self {
        Self::SessionPersistence(SessionPersistenceError::Json(err))
    }
}

/// Allows `?` to convert ACP protocol errors encountered inside domain code
/// into [`AdapterError::Internal`].
impl From<agent_client_protocol::Error> for AdapterError {
    fn from(err: agent_client_protocol::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SessionPersistenceError — Display
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn session_persistence_error_state_dir_display() {
        let err = SessionPersistenceError::StateDir("no home".into());
        let msg = err.to_string();
        assert!(msg.contains("failed to resolve state directory"));
        assert!(msg.contains("no home"));
    }

    #[test_log::test]
    fn session_persistence_error_invalid_session_id_display() {
        let err = SessionPersistenceError::InvalidSessionId("bad/id".into());
        let msg = err.to_string();
        assert!(msg.contains("invalid persisted session id"));
        assert!(msg.contains("bad/id"));
    }

    #[test_log::test]
    fn session_persistence_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = SessionPersistenceError::Io(io_err);
        let msg = err.to_string();
        assert!(msg.contains("filesystem session store I/O failed"));
        assert!(msg.contains("file not found"));
    }

    #[test_log::test]
    fn session_persistence_error_json_display() {
        // Construct a serde_json::Error via the public io() constructor.
        let json_err = serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid syntax",
        ));
        let err = SessionPersistenceError::Json(json_err);
        let msg = err.to_string();
        assert!(msg.contains("filesystem session store JSON failed"));
        assert!(msg.contains("invalid syntax"));
    }

    // -----------------------------------------------------------------------
    // SessionPersistenceError — Debug
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn session_persistence_error_debug_impl() {
        let err = SessionPersistenceError::StateDir("test".into());
        let debug = format!("{err:?}");
        assert!(debug.contains("StateDir"));
    }

    // -----------------------------------------------------------------------
    // AdapterError — Display
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn adapter_error_deepseek_display() {
        let deepseek_err = DeepSeekError::MissingApiKey;
        let err = AdapterError::DeepSeek(deepseek_err);
        let msg = err.to_string();
        assert!(msg.contains("DeepSeek API error"));
        assert!(msg.contains("DEEPSEEK_API_KEY is not set"));
    }

    #[test_log::test]
    fn adapter_error_session_persistence_display() {
        let persist_err = SessionPersistenceError::StateDir("missing".into());
        let err = AdapterError::SessionPersistence(persist_err);
        let msg = err.to_string();
        assert!(msg.contains("session persistence error"));
        assert!(msg.contains("failed to resolve state directory"));
        assert!(msg.contains("missing"));
    }

    #[test_log::test]
    fn adapter_error_invalid_params_display() {
        let err = AdapterError::InvalidParams("bad param".into());
        let msg = err.to_string();
        assert!(msg.contains("invalid params"));
        assert!(msg.contains("bad param"));
    }

    #[test_log::test]
    fn adapter_error_invalid_request_display() {
        let err = AdapterError::InvalidRequest("bad request".into());
        let msg = err.to_string();
        assert!(msg.contains("invalid request"));
        assert!(msg.contains("bad request"));
    }

    #[test_log::test]
    fn adapter_error_session_not_found_display() {
        let err = AdapterError::SessionNotFound("sess-1".into());
        let msg = err.to_string();
        assert!(msg.contains("session not found"));
        assert!(msg.contains("sess-1"));
    }

    #[test_log::test]
    fn adapter_error_internal_display() {
        let err = AdapterError::Internal("something broke".into());
        let msg = err.to_string();
        assert!(msg.contains("internal error"));
        assert!(msg.contains("something broke"));
    }

    // -----------------------------------------------------------------------
    // AdapterError — Debug
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn adapter_error_debug_impl() {
        let err = AdapterError::InvalidParams("test".into());
        let debug = format!("{err:?}");
        assert!(debug.contains("InvalidParams"));
    }

    // -----------------------------------------------------------------------
    // From<AdapterError> for agent_client_protocol::Error
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn from_adapter_error_invalid_params_to_acp_error() {
        let adapter_err = AdapterError::InvalidParams("missing field".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("missing field"));
    }

    #[test_log::test]
    fn from_adapter_error_invalid_request_to_acp_error() {
        let adapter_err = AdapterError::InvalidRequest("bad json".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("bad json"));
    }

    #[test_log::test]
    fn from_adapter_error_session_not_found_to_acp_error() {
        let adapter_err = AdapterError::SessionNotFound("sess-42".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("session not found"));
        assert!(msg.contains("sess-42"));
    }

    #[test_log::test]
    fn from_adapter_error_deepseek_to_acp_internal_error() {
        let adapter_err = AdapterError::DeepSeek(DeepSeekError::MissingApiKey);
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        // DeepSeek variant hits the `other => into_internal_error` arm.
        assert!(msg.contains("DEEPSEEK_API_KEY is not set") || msg.contains("MissingApiKey"));
    }

    #[test_log::test]
    fn from_adapter_error_session_persistence_to_acp_internal_error() {
        let persist_err = SessionPersistenceError::StateDir("no-dir".into());
        let adapter_err = AdapterError::SessionPersistence(persist_err);
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("failed to resolve state directory"));
    }

    #[test_log::test]
    fn from_adapter_error_internal_to_acp_internal_error() {
        let adapter_err = AdapterError::Internal("assertion failed".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("assertion failed"));
    }

    // -----------------------------------------------------------------------
    // From<std::io::Error> for AdapterError
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn from_io_error_to_adapter_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let adapter_err: AdapterError = io_err.into();
        let msg = adapter_err.to_string();
        assert!(msg.contains("filesystem session store I/O failed"));
        assert!(msg.contains("permission denied"));
    }

    // -----------------------------------------------------------------------
    // From<serde_json::Error> for AdapterError
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn from_serde_json_error_to_adapter_error() {
        let json_err = serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad json",
        ));
        let adapter_err: AdapterError = json_err.into();
        let msg = adapter_err.to_string();
        assert!(msg.contains("filesystem session store JSON failed"));
        assert!(msg.contains("bad json"));
    }

    // -----------------------------------------------------------------------
    // From<agent_client_protocol::Error> for AdapterError
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn from_acp_error_to_adapter_error() {
        let acp_err = agent_client_protocol::Error::invalid_params().data("oops");
        let adapter_err: AdapterError = acp_err.into();
        let msg = adapter_err.to_string();
        assert!(msg.contains("internal error"));
        assert!(msg.contains("oops"));
    }

    // -----------------------------------------------------------------------
    // #[from] attribute conversions (derive-generated)
    // -----------------------------------------------------------------------

    #[test_log::test]
    fn deepseek_error_into_adapter_error_via_from() {
        let deepseek_err = DeepSeekError::MissingApiKey;
        let adapter_err: AdapterError = deepseek_err.into();
        assert!(matches!(adapter_err, AdapterError::DeepSeek(_)));
    }

    #[test_log::test]
    fn session_persistence_error_into_adapter_error_via_from() {
        let persist_err = SessionPersistenceError::StateDir("x".into());
        let adapter_err: AdapterError = persist_err.into();
        assert!(matches!(adapter_err, AdapterError::SessionPersistence(_)));
    }

    #[test_log::test]
    fn std_io_error_into_session_persistence_error_via_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let persist_err: SessionPersistenceError = io_err.into();
        assert!(matches!(persist_err, SessionPersistenceError::Io(_)));
    }

    #[test_log::test]
    fn serde_json_error_into_session_persistence_error_via_from() {
        let json_err = serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "custom error",
        ));
        let persist_err: SessionPersistenceError = json_err.into();
        assert!(matches!(persist_err, SessionPersistenceError::Json(_)));
    }

    // -----------------------------------------------------------------------
    // Send + Sync (auto-derived, compile-time check)
    // -----------------------------------------------------------------------

    /// Compile-time assertion that `SessionPersistenceError` is `Send + Sync`.
    fn session_persistence_error_is_send_sync()
    where
        SessionPersistenceError: Send + Sync,
    {
    }

    /// Compile-time assertion that `AdapterError` is `Send + Sync`.
    fn adapter_error_is_send_sync()
    where
        AdapterError: Send + Sync,
    {
    }

    #[test_log::test]
    fn error_types_are_send_and_sync() {
        // The functions above assert at compile time that the types implement
        // Send + Sync.  Call them at runtime to keep coverage instrumentation
        // happy.
        session_persistence_error_is_send_sync();
        adapter_error_is_send_sync();
    }
}
