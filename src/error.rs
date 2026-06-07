//! Unified error type for the `DeepSeek` ACP adapter.
//!
//! `AdapterError` wraps all domain-level errors produced by the adapter so
//! that callers (primarily the ACP protocol boundary) can convert a single
//! error type into [`agent_client_protocol::Error`] without matching on every
//! internal error variant.

use thiserror::Error;

use crate::deepseek::DeepSeekError;

// ---------------------------------------------------------------------------
// SessionPersistenceError — moved here from the binary so AdapterError can
// use `#[from]` without crate-boundary headaches.
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
/// | `Internal` | — | Unexpected internal invariant violations |
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The `DeepSeek` API client returned an error.
    #[error("DeepSeek API error: {0}")]
    DeepSeek(#[from] DeepSeekError),

    /// Session persistence (filesystem I/O, JSON) failed.
    #[error("session persistence error: {0}")]
    SessionPersistence(#[from] SessionPersistenceError),

    /// An unexpected internal invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<AdapterError> for agent_client_protocol::Error {
    /// Converts any [`AdapterError`] into an ACP internal error.
    ///
    /// The error's `Display` message is attached as JSON-RPC error data so
    /// that the client receives diagnostic information without leaking
    /// implementation details into the error code / message fields.
    fn from(err: AdapterError) -> Self {
        agent_client_protocol::Error::into_internal_error(err)
    }
}
