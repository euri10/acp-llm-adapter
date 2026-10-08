//! Confined local filesystem access and canonical paths for trusted editor I/O.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::fs::Dir;

#[derive(Debug, thiserror::Error)]
pub(crate) enum FileAccessError {
    #[error("file path is outside session roots")]
    OutsideRoots,
    #[error("file path cannot be verified: {0}")]
    Io(#[from] io::Error),
}

/// A canonical display path paired with the directory capability used for I/O.
#[derive(Debug, Clone)]
pub(crate) struct ConfinedPath {
    pub(crate) path: PathBuf,
    root_path: PathBuf,
    root: Arc<Dir>,
    relative: PathBuf,
}

impl ConfinedPath {
    pub(crate) fn resolve(
        cwd: &Path,
        additional: &[PathBuf],
        path: &Path,
    ) -> Result<Self, FileAccessError> {
        let mut candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        if !path.is_absolute()
            && matches!(std::fs::symlink_metadata(&candidate), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            for root in additional {
                let alternate = root.join(path);
                if std::fs::symlink_metadata(&alternate).is_ok() {
                    candidate = alternate;
                    break;
                }
            }
        }
        for root_path in std::iter::once(cwd).chain(additional.iter().map(PathBuf::as_path)) {
            let root_path = std::fs::canonicalize(root_path)?;
            let root = Arc::new(Dir::open_ambient_dir(
                &root_path,
                cap_std::ambient_authority(),
            )?);
            let path = canonicalize_new(&candidate)?;
            if let Ok(relative) = path.strip_prefix(&root_path) {
                let relative = if relative.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    relative.to_path_buf()
                };
                // Ambient authority is granted only to an explicitly approved root.
                // All subsequent operations stay relative to this owned handle:
                // https://docs.rs/cap-std/4.0.3/cap_std/fs/struct.Dir.html
                let resolved = Self {
                    path,
                    root_path,
                    root,
                    relative,
                };
                resolved.path_for_client()?;
                return Ok(resolved);
            }
        }
        Err(FileAccessError::OutsideRoots)
    }

    pub(crate) fn read_to_string(&self) -> io::Result<String> {
        self.root.read_to_string(&self.relative)
    }

    pub(crate) fn write(&self, content: &str) -> io::Result<()> {
        self.root.write(&self.relative, content)
    }

    pub(crate) fn directory(&self) -> io::Result<Dir> {
        self.root.open_dir(&self.relative)
    }

    /// Recheck the pinned root immediately before each delegated operation.
    pub(crate) fn path_for_client(&self) -> Result<PathBuf, FileAccessError> {
        let mut existing = self.relative.clone();
        let mut missing = Vec::new();
        loop {
            match self.root.canonicalize(&existing) {
                Ok(mut canonical) => {
                    for part in missing.iter().rev() {
                        canonical.push(part);
                    }
                    let path = self.root_path.join(canonical);
                    if canonicalize_new(&path)? != path {
                        return Err(FileAccessError::OutsideRoots);
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if self
                        .root
                        .symlink_metadata(&existing)
                        .is_ok_and(|metadata| metadata.is_symlink())
                    {
                        return Err(error.into());
                    }
                    let name = existing.file_name().ok_or(error)?.to_os_string();
                    missing.push(name);
                    existing.pop();
                    if existing.as_os_str().is_empty() {
                        existing.push(".");
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// Resolve symlinks before normalizing the not-yet-existing suffix of a new file.
fn canonicalize_new(path: &Path) -> Result<PathBuf, FileAccessError> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    match component {
                        Component::Normal(name) => canonical.push(name),
                        Component::CurDir => {}
                        Component::ParentDir => {
                            if !canonical.pop() {
                                return Err(FileAccessError::OutsideRoots);
                            }
                        }
                        _ => return Err(FileAccessError::OutsideRoots),
                    }
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&existing).is_ok_and(|metadata| metadata.is_symlink())
                {
                    return Err(error.into());
                }
                // Borrow from the original path, not the shrinking buffer.
                let remaining = existing.components().count();
                let component = path
                    .components()
                    .nth(remaining.saturating_sub(1))
                    .ok_or(error)?;
                missing.push(component);
                if !existing.pop() {
                    return Err(FileAccessError::OutsideRoots);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}
