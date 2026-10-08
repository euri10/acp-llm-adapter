//! Cwd-only traversal using directory capabilities, never ambient path opens.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, DirEntry, File};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tokio_util::sync::CancellationToken;

use super::filesystem::ConfinedPath;

struct DirectoryEntries {
    path: PathBuf,
    entries: std::vec::IntoIter<DirEntry>,
    gitignore: Option<Gitignore>,
    ignore: Option<Gitignore>,
}

impl DirectoryEntries {
    fn new(
        directory: &Dir,
        root: &Dir,
        path: PathBuf,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        let gitignore = load_ignore(root, &path, ".gitignore")?;
        let ignore = load_ignore(root, &path, ".ignore")?;
        let mut entries = Vec::new();
        for entry in directory.entries().map_err(|error| error.to_string())? {
            check_cancelled(cancellation)?;
            entries.push(entry.map_err(|error| error.to_string())?);
        }
        entries.sort_unstable_by_key(DirEntry::file_name);
        Ok(Self {
            path,
            entries: entries.into_iter(),
            gitignore,
            ignore,
        })
    }
}

fn load_ignore(root: &Dir, directory: &Path, name: &str) -> Result<Option<Gitignore>, String> {
    let path = directory.join(name);
    let text = match root.read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // An absent config is normal; a dangling link is not an absent file.
            if root
                .symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.is_symlink())
            {
                return Err(format!("cannot verify ignore file {}", path.display()));
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "failed to read ignore file {}: {error}",
                path.display()
            ));
        }
    };
    // add_line parses text already read through the capability; add(path) would
    // reopen it with ambient authority. Do not load global or parent rules.
    // https://docs.rs/ignore/0.4.33/ignore/gitignore/struct.GitignoreBuilder.html
    let mut builder = GitignoreBuilder::new(directory);
    for line in text.lines() {
        builder
            .add_line(Some(path.clone()), line)
            .map_err(|error| error.to_string())?;
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

/// Visit regular, visible, unignored files in deterministic depth-first order.
/// Returning false from the visitor stops traversal (for output caps).
pub(crate) fn walk_files(
    root: &ConfinedPath,
    cancellation: &CancellationToken,
    mut visit: impl FnMut(&Path, &DirEntry) -> Result<bool, String>,
) -> Result<(), String> {
    check_cancelled(cancellation)?;
    let directory = root.directory().map_err(|error| error.to_string())?;
    let mut stack = vec![DirectoryEntries::new(
        &directory,
        &directory,
        PathBuf::new(),
        cancellation,
    )?];
    while let Some(current) = stack.last_mut() {
        check_cancelled(cancellation)?;
        let Some(entry) = current.entries.next() else {
            stack.pop();
            continue;
        };
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = current.path.join(name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        // .ignore takes precedence over .gitignore; within each category the
        // nearest rule wins, including negations. Ignored dirs are never opened.
        let ignored = stack
            .iter()
            .rev()
            .filter_map(|frame| frame.ignore.as_ref())
            .chain(
                stack
                    .iter()
                    .rev()
                    .filter_map(|frame| frame.gitignore.as_ref()),
            )
            .map(|rules| rules.matched(&path, kind.is_dir()))
            .find(|matched| !matched.is_none())
            .is_some_and(|matched| matched.is_ignore());
        if ignored {
            continue;
        }
        if kind.is_dir() {
            let child = entry.open_dir().map_err(|error| error.to_string())?;
            stack.push(DirectoryEntries::new(
                &child,
                &directory,
                path,
                cancellation,
            )?);
        } else if !visit(&path, &entry)? {
            break;
        }
    }
    Ok(())
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("file search cancelled".to_owned())
    } else {
        Ok(())
    }
}

/// Check cancellation between reads, including files with no matching lines.
pub(crate) struct SearchReader<'a> {
    pub(crate) file: File,
    pub(crate) cancellation: &'a CancellationToken,
}

impl Read for SearchReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_cancelled(self.cancellation).map_err(io::Error::other)?;
        self.file.read(buffer)
    }
}
