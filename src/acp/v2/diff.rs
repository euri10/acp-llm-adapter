//! v2 changes-list diffs, shared by approval previews and completed edits.

use agent_client_protocol::schema::v2;
use std::fmt::Write;
use std::path::Path;

pub(super) fn content(path: &Path, old: Option<&str>, new: &str) -> v2::ToolCallContent {
    let change = if old.is_some() {
        v2::DiffChange::modify(path.to_path_buf())
    } else {
        v2::DiffChange::add(path.to_path_buf())
    }
    .file_type(v2::DiffFileType::Text);
    let diff = v2::Diff::new(vec![change]);
    v2::ToolCallContent::Diff(if old == Some(new) {
        diff
    } else {
        diff.with_patch(v2::DiffPatch::new(patch(path, old, new)))
    })
}

/// ACP requires absolute paths and Git quoting, without a/ or b/ prefixes:
/// <https://agentclientprotocol.com/protocol/v2/tool-calls#diffs>
fn quote_path(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            32..=126 => quoted.push(char::from(byte)),
            // Writing into a String is infallible.
            _ => {
                let _ = write!(quoted, "\\{byte:03o}");
            }
        }
    }
    quoted.push('"');
    quoted
}

fn patch(path: &Path, old: Option<&str>, new: &str) -> String {
    let path = quote_path(path);
    let mut output = format!("diff --git {path} {path}\n");
    if old.is_none() {
        output.push_str("new file mode 100644\n");
    }
    let old_path = if old.is_some() {
        path.as_str()
    } else {
        "/dev/null"
    };
    let before = old.unwrap_or("");
    let old_count = before.split_inclusive('\n').count();
    let new_count = new.split_inclusive('\n').count();
    // Explicit filenames also make an empty-file creation renderable.
    let _ = write!(output, "--- {old_path}\n+++ {path}\n");
    if old_count + new_count == 0 {
        return output;
    }
    // ponytail: one whole-file hunk avoids a diff dependency; use an existing
    // diff engine if very large previews become a measured UI bottleneck.
    let _ = writeln!(
        output,
        "@@ -{},{old_count} +{},{new_count} @@",
        usize::from(old_count > 0),
        usize::from(new_count > 0)
    );
    for (prefix, text) in [('-', before), ('+', new)] {
        for line in text.split_inclusive('\n') {
            output.push(prefix);
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_files_have_no_renderable_patch() {
        let v2::ToolCallContent::Diff(diff) = content(Path::new("/tmp/empty"), Some(""), "") else {
            unreachable!("content always emits a diff");
        };
        assert!(diff.patch.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn git_can_apply_creation_and_replacement_patches() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let root = std::env::temp_dir().join(format!("v2-diff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        for (index, (before, after)) in [
            (None, ""),
            (None, "new"),
            (Some(""), "x\n"),
            (Some("old\n"), ""),
            (Some("old\r\nlast"), "new\r\nlast\n"),
        ]
        .into_iter()
        .enumerate()
        {
            let path = root.join(format!("file {index}\t\"é.txt"));
            if let Some(text) = before {
                std::fs::write(&path, text)?;
            }
            let mut child = Command::new("git")
                .args(["apply", "-p0", "--unsafe-paths", "--whitespace=nowarn"])
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .ok_or("no git stdin")?
                .write_all(patch(&path, before, after).as_bytes())?;
            let output = child.wait_with_output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(&path)?, after);
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn patches_preserve_missing_newlines_and_quote_paths() {
        let path = Path::new("/tmp/a\t\"é.txt");
        let rendered = patch(path, Some("old"), "new\n");
        assert!(rendered.contains("\"/tmp/a\\011\\\"\\303\\251.txt\""));
        assert!(rendered.ends_with("@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n"));
        assert!(patch(Path::new("/tmp/new"), None, "").contains("new file mode 100644"));
        assert!(patch(Path::new("/tmp/new"), None, "x").contains("--- /dev/null\n"));
        assert!(patch(Path::new("/tmp/empty"), Some("x\n"), "").contains("@@ -1,1 +0,0 @@\n"));
    }
}
