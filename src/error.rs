//! Unified error type for the `acp-llm-adapter`.
//!
//! `AdapterError` wraps all domain-level errors produced by the adapter so
//! that callers (primarily the ACP protocol boundary) can convert a single
//! error type into [`agent_client_protocol::Error`] without matching on every
//! internal error variant.

use thiserror::Error;

use crate::llm::ChatError;

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

/// Unified domain error for the `acp-llm-adapter`.
///
/// Every public fallible function in the domain layer returns this type (or a
/// `Result` whose error variant is this type).  The ACP boundary owns the
/// single `From` implementation that converts `AdapterError` into
/// [`agent_client_protocol::Error`].
/// Internal [`Display`](std::fmt::Display) and source errors retain diagnostic
/// detail; ACP conversion exposes only fixed, reviewed messages.
///
/// # Errors
///
/// Each variant corresponds to a specific failure domain:
///
/// | Variant | Source | Typical cause |
/// |---|---|---|
/// | `Llm` | [`ChatError`] | API key missing, transport failure, bad response |
/// | `SessionPersistence` | [`SessionPersistenceError`] | I/O, JSON, invalid session id |
/// | `InvalidParams` | — | Invalid method parameters |
/// | `InvalidRequest` | — | Invalid request structure |
/// | `SessionNotFound` | — | Session id not found in store |
/// | `Internal` | — | Unexpected internal invariant violations |
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The LLM client returned an error.
    #[error("LLM API error: {0}")]
    Llm(#[from] ChatError),

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
    /// JSON-RPC error code and a fixed message without private input or causes.
    fn from(err: AdapterError) -> Self {
        match err {
            AdapterError::InvalidParams(msg) => agent_client_protocol::Error::invalid_params()
                .data(validation_message(&msg, "invalid method parameters")),
            AdapterError::InvalidRequest(msg) => agent_client_protocol::Error::invalid_request()
                .data(validation_message(&msg, "invalid request")),
            AdapterError::SessionNotFound(_) => {
                agent_client_protocol::Error::invalid_params().data("session not found")
            }
            AdapterError::Llm(error) => {
                agent_client_protocol::Error::internal_error().data(match error {
                    ChatError::MissingApiKey => "provider API key is not configured",
                    ChatError::Transport(_) => "provider connection failed",
                    ChatError::InvalidResponse(_) | ChatError::Json(_) => {
                        "provider returned an invalid response"
                    }
                })
            }
            AdapterError::SessionPersistence(_) => {
                agent_client_protocol::Error::internal_error().data("session storage failed")
            }
            AdapterError::Internal(_) => {
                agent_client_protocol::Error::internal_error().data("adapter operation failed")
            }
        }
    }
}

/// Select only static diagnostic text. Domain validation strings may contain
/// peer values or wrapped MCP failures, so even recognized prefixes are never
/// returned from the original string. Unrecognized errors fail closed.
fn validation_message(detail: &str, fallback: &'static str) -> &'static str {
    if detail.starts_with("MCP server '") {
        return "MCP server command must be absolute";
    }
    if detail.starts_with("session ") && detail.ends_with(" already has an active turn") {
        return "session already has an active turn";
    }
    if detail.starts_with("tool call delta ") {
        return "provider tool call is incomplete";
    }
    [
        "unknown session id",
        "unknown permission option selected",
        "unsupported permission outcome variant",
        "unsupported max_tokens value",
        "unsupported model",
        "unsupported MCP server transport",
        "failed to start MCP server",
        "failed to initialize MCP server",
        "failed to list MCP tools",
        "invalid HTTP header name",
        "invalid HTTP header value",
        "binary resource prompt blocks are not supported",
        "unsupported embedded resource prompt block",
        "only text, resource link, and text resource prompt blocks are supported",
        "prompt must include non-empty text",
        "session/load requires filesystem persistence",
        "selected-content prompts require text snapshots",
        "selected-content deadline exceeded",
        "selected-content output byte limit exceeded",
        "selected-content Sessions refuse all tool calls",
        "selected-content Sessions allow one attempt",
        "selected-content input byte limit exceeded",
    ]
    .into_iter()
    .find(|message| detail.starts_with(message))
    .unwrap_or(fallback)
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
    use std::error::Error as _;

    use agent_client_protocol::ErrorCode::{InternalError, InvalidParams, InvalidRequest};

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
    fn adapter_error_llm_display() {
        let deepseek_err = ChatError::MissingApiKey;
        let err = AdapterError::Llm(deepseek_err);
        let msg = err.to_string();
        assert!(msg.contains("LLM API error"));
        assert!(msg.contains("LLM_API_KEY is not set"));
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
        assert_eq!(
            acp_err.code,
            agent_client_protocol::ErrorCode::InvalidParams
        );
        assert_eq!(acp_err.data, Some("invalid method parameters".into()));
    }

    #[test_log::test]
    fn from_adapter_error_invalid_request_to_acp_error() {
        let adapter_err = AdapterError::InvalidRequest("bad json".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        assert_eq!(
            acp_err.code,
            agent_client_protocol::ErrorCode::InvalidRequest
        );
        assert_eq!(acp_err.data, Some("invalid request".into()));
    }

    #[test_log::test]
    fn from_adapter_error_session_not_found_to_acp_error() {
        let adapter_err = AdapterError::SessionNotFound("sess-42".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        let msg = acp_err.to_string();
        assert!(msg.contains("session not found"));
        assert!(!msg.contains("sess-42"));
    }

    #[test_log::test]
    fn from_adapter_error_llm_to_acp_internal_error() {
        let adapter_err = AdapterError::Llm(ChatError::MissingApiKey);
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        assert_eq!(
            acp_err.code,
            agent_client_protocol::ErrorCode::InternalError
        );
        assert_eq!(
            acp_err.data,
            Some("provider API key is not configured".into())
        );
    }

    #[test_log::test]
    fn from_adapter_error_session_persistence_to_acp_internal_error() {
        let persist_err = SessionPersistenceError::StateDir("no-dir".into());
        let adapter_err = AdapterError::SessionPersistence(persist_err);
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        assert_eq!(
            acp_err.code,
            agent_client_protocol::ErrorCode::InternalError
        );
        assert_eq!(acp_err.data, Some("session storage failed".into()));
    }

    #[test_log::test]
    fn from_adapter_error_internal_to_acp_internal_error() {
        let adapter_err = AdapterError::Internal("assertion failed".into());
        let acp_err: agent_client_protocol::Error = adapter_err.into();
        assert_eq!(
            acp_err.code,
            agent_client_protocol::ErrorCode::InternalError
        );
        assert_eq!(acp_err.data, Some("adapter operation failed".into()));
    }

    #[test_log::test]
    fn acp_presentation_never_serializes_private_error_details()
    -> Result<(), Box<dyn std::error::Error>> {
        let sentinel = "PRIVATE_ERROR_SENTINEL";
        let json_error = serde_json::from_value::<u64>(serde_json::json!(sentinel))
            .err()
            .ok_or("expected a JSON type error")?;
        let provider: AdapterError = ChatError::Json(json_error).into();
        let storage: AdapterError =
            std::io::Error::other(format!("/private/{sentinel}/sessions")).into();
        let transport: AdapterError = ChatError::Transport(Box::new(std::io::Error::other(
            format!("https://example.invalid/?token={sentinel}"),
        )))
        .into();
        for (error, code, message) in [
            (
                provider,
                InternalError,
                "provider returned an invalid response",
            ),
            (transport, InternalError, "provider connection failed"),
            (storage, InternalError, "session storage failed"),
            (
                AdapterError::Llm(ChatError::InvalidResponse(sentinel.into())),
                InternalError,
                "provider returned an invalid response",
            ),
            (
                AdapterError::InvalidParams(sentinel.into()),
                InvalidParams,
                "invalid method parameters",
            ),
            (
                AdapterError::InvalidRequest(sentinel.into()),
                InvalidRequest,
                "invalid request",
            ),
            (
                AdapterError::SessionNotFound(sentinel.into()),
                InvalidParams,
                "session not found",
            ),
            (
                AdapterError::Internal(sentinel.into()),
                InternalError,
                "adapter operation failed",
            ),
            (
                AdapterError::InvalidParams(format!(
                    "invalid HTTP header name '{sentinel}' for MCP server '{sentinel}': invalid header"
                )),
                InvalidParams,
                "invalid HTTP header name",
            ),
            (
                AdapterError::InvalidParams(format!(
                    "invalid HTTP header value for 'Authorization' on MCP server '{sentinel}': {sentinel}"
                )),
                InvalidParams,
                "invalid HTTP header value",
            ),
            (
                AdapterError::InvalidParams(format!(
                    "failed to initialize MCP server '{sentinel}': https://example.invalid/?token={sentinel}"
                )),
                InvalidParams,
                "failed to initialize MCP server",
            ),
            (
                AdapterError::InvalidParams(format!("unsupported model: {sentinel}")),
                InvalidParams,
                "unsupported model",
            ),
        ] {
            assert!(error.to_string().contains(sentinel));
            let presented: agent_client_protocol::Error = error.into();
            assert_eq!(presented.code, code);
            assert_eq!(presented.data, Some(message.into()));
            assert!(!serde_json::to_string(&presented)?.contains(sentinel));
            assert!(!format!("{presented:?}").contains(sentinel));
        }
        Ok(())
    }

    #[test_log::test]
    fn domain_errors_retain_typed_private_causes() -> Result<(), Box<dyn std::error::Error>> {
        let sentinel = "PRIVATE_CAUSE_SENTINEL";
        let json_error = serde_json::from_value::<u64>(serde_json::json!(sentinel))
            .err()
            .ok_or("expected a JSON type error")?;
        let provider: AdapterError = ChatError::Json(json_error).into();
        let llm = provider.source().ok_or("missing LLM cause")?;
        assert!(llm.downcast_ref::<ChatError>().is_some());
        let json = llm.source().ok_or("missing JSON cause")?;
        assert!(json.downcast_ref::<serde_json::Error>().is_some());
        assert!(json.to_string().contains(sentinel));

        for error in [
            AdapterError::from(std::io::Error::other(sentinel)),
            AdapterError::from(ChatError::Transport(Box::new(std::io::Error::other(
                sentinel,
            )))),
        ] {
            let cause = error
                .source()
                .ok_or("missing domain cause")?
                .source()
                .ok_or("missing I/O cause")?;
            assert!(cause.downcast_ref::<std::io::Error>().is_some());
            assert!(cause.to_string().contains(sentinel));
        }
        Ok(())
    }

    #[test_log::test]
    fn acp_presentation_preserves_safe_validation_diagnostics() {
        for message in [
            "prompt must include non-empty text",
            "selected-content Sessions refuse all tool calls",
            "selected-content input byte limit exceeded",
        ] {
            let error = if message.contains("refuse") {
                AdapterError::InvalidRequest(message.into())
            } else {
                AdapterError::InvalidParams(message.into())
            };
            let presented: agent_client_protocol::Error = error.into();
            assert_eq!(presented.data, Some(message.into()));
        }
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
    fn llm_error_into_adapter_error_via_from() {
        let deepseek_err = ChatError::MissingApiKey;
        let adapter_err: AdapterError = deepseek_err.into();
        assert!(matches!(adapter_err, AdapterError::Llm(_)));
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
