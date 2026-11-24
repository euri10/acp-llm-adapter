//! Well-known filesystem locations.
//!
//! Lives in the library rather than the adapter binary so the proxy binary
//! resolves the same locations without a second copy of the rules.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Directory name this application owns inside the host's state directory.
pub const APPLICATION_STATE_DIR: &str = "acp-llm-adapter";

/// Subdirectory holding logs produced while proxying a foreign ACP agent.
///
/// Deliberately separate from the adapter's own session store: the store
/// enumerates its `sessions` directory, so proxied sessions written alongside
/// it would either accumulate as invisible clutter or surface as resumable
/// adapter sessions, neither of which is true.
pub const PROXY_DIR: &str = "proxy";

/// Create application-owned directories privately, from `root` through `path`.
/// Existing directories outside `root` are left unchanged.
///
/// # Errors
///
/// Returns an error if `path` is outside `root` or a directory cannot be created
/// or made private.
pub fn create_private_dirs(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory is outside state root",
        )
    })?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid state directory",
        ));
    }
    create_missing_dirs(root)?;
    make_private_dir(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        create_missing_dirs(&current)?;
        make_private_dir(&current)?;
    }
    Ok(())
}

fn create_missing_dirs(path: &Path) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_missing_dirs(parent)?;
    }
    #[cfg(unix)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => make_private_dir(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn make_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    #[cfg(not(unix))]
    fs::metadata(path).map(|_| ())?;
    Ok(())
}

/// Open an application state file with owner-only access on Unix.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or its permissions cannot be
/// restricted.
pub fn open_private_file(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

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
