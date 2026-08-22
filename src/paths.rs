//! Well-known filesystem locations.
//!
//! Lives in the library rather than the adapter binary so the proxy binary
//! resolves the same locations without a second copy of the rules.

use std::path::PathBuf;

/// Directory name this application owns inside the host's state directory.
pub const APPLICATION_STATE_DIR: &str = "acp-llm-adapter";

/// Subdirectory holding logs produced while proxying a foreign ACP agent.
///
/// Deliberately separate from the adapter's own session store: the store
/// enumerates its `sessions` directory, so proxied sessions written alongside
/// it would either accumulate as invisible clutter or surface as resumable
/// adapter sessions, neither of which is true.
pub const PROXY_DIR: &str = "proxy";

/// Resolve this application's state directory.
///
/// Honours `XDG_STATE_HOME`, falling back to `$HOME/.local/state`. Returns
/// `None` when the environment exposes neither, leaving the caller to raise
/// whichever error suits its domain.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(path).join(APPLICATION_STATE_DIR));
    }

    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join(APPLICATION_STATE_DIR),
    )
}

/// Resolve the root that proxy-mode logs are written under.
///
/// See [`PROXY_DIR`] for why this is not the adapter's own state directory.
#[must_use]
pub fn default_proxy_log_root() -> Option<PathBuf> {
    Some(default_state_dir()?.join(PROXY_DIR))
}

#[cfg(test)]
mod tests;
