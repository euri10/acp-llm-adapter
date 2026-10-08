//! File authority is independent of permission mode and filesystem transport.

use super::*;
use crate::acp::handle_new_session_request;
use crate::test_utils::{
    CountingReadTextFileRequester, FakePermissionRequester, RecordingWriteTextFileRequester,
};
use agent_client_protocol::schema::v1::{
    ClientCapabilities, FileSystemCapabilities, NewSessionRequest,
};
use std::fs;

struct Workspace {
    root: PathBuf,
    outside: PathBuf,
    extra: PathBuf,
    context: ToolContext,
    store: SessionStore,
}

impl Workspace {
    fn new(delegated: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("acp-confine-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let outside = root.join("project-neighbor");
        let extra = root.join("extra");
        for dir in [&cwd, &outside, &extra] {
            fs::create_dir_all(dir)?;
        }
        fs::write(outside.join("secret.txt"), "private sentinel")?;
        fs::write(cwd.join("inside.txt"), "inside")?;
        fs::write(extra.join("extra.txt"), "extra")?;
        let store = crate::test_store();
        let session = handle_new_session_request(&store, &NewSessionRequest::new(&cwd))?;
        store.set_mode(&session.session_id, SessionBehavior::Yolo)?;
        let context = ToolContext {
            session_id: session.session_id,
            cwd,
            additional_directories: vec![extra.clone()],
            client_capabilities: Some(
                ClientCapabilities::new().fs(FileSystemCapabilities::new()
                    .read_text_file(delegated)
                    .write_text_file(delegated)),
            ),
        };
        Ok(Self {
            root,
            outside,
            extra,
            context,
            store,
        })
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test_log::test(tokio::test)]
async fn file_roots_reject_absolute_traversal_and_prefix_siblings_before_io()
-> Result<(), Box<dyn std::error::Error>> {
    for delegated in [false, true] {
        let workspace = Workspace::new(delegated)?;
        let read = CountingReadTextFileRequester::new();
        let write = RecordingWriteTextFileRequester::new();
        let permission = FakePermissionRequester::new(vec![]);
        let token = CancellationToken::new();
        for mode in [
            SessionBehavior::Ask,
            SessionBehavior::AcceptEdits,
            SessionBehavior::Plan,
            SessionBehavior::Yolo,
        ] {
            workspace
                .store
                .set_mode(&workspace.context.session_id, mode)?;
            for path in [
                workspace.outside.join("secret.txt"),
                PathBuf::from("../project-neighbor/secret.txt"),
            ] {
                for name in ["read_file", "write_file", "edit_file"] {
                    let call = ChatToolCall::new("denied", name, serde_json::json!({
                    "path": path, "content": "overwritten", "old_text": "private sentinel", "new_text": "overwritten"
                }).to_string());
                    let result = match name {
                        "read_file" => {
                            read_file_tool_execution(&call, &workspace.context, Some(&read)).await
                        }
                        "write_file" => {
                            write_file_tool_execution(
                                &workspace.store,
                                &call,
                                &workspace.context,
                                Some(&read),
                                Some(&write),
                                Some(&permission),
                                &token,
                            )
                            .await
                        }
                        _ => {
                            edit_file_tool_execution(
                                &workspace.store,
                                &call,
                                &workspace.context,
                                Some(&read),
                                Some(&write),
                                Some(&permission),
                                &token,
                            )
                            .await
                        }
                    };
                    assert!(
                        !result.success,
                        "{name} accepted outside path {path:?}: {}",
                        result.content
                    );
                    assert!(
                        result.content.contains("outside session roots"),
                        "wrong rejection: {}",
                        result.content
                    );
                }
            }
        }
        let result = list_dir_tool_execution(
            &ChatToolCall::new(
                "list",
                "list_dir",
                serde_json::json!({"path":workspace.outside}).to_string(),
            ),
            &workspace.context,
        );
        assert!(!result.success);
        assert_eq!(
            fs::read_to_string(workspace.outside.join("secret.txt"))?,
            "private sentinel"
        );
        assert_eq!(*read.calls().lock().map_err(|e| e.to_string())?, 0);
        assert!(
            permission
                .requests()
                .lock()
                .map_err(|e| e.to_string())?
                .is_empty()
        );
        assert!(
            write
                .requests()
                .lock()
                .map_err(|e| e.to_string())?
                .is_empty()
        );
    }
    Ok(())
}

#[cfg(unix)]
struct SwapOnApproval {
    path: PathBuf,
    outside: PathBuf,
}

#[cfg(unix)]
impl PermissionRequester for SwapOnApproval {
    fn request_permission(
        &self,
        _request: agent_client_protocol::schema::v1::RequestPermissionRequest,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<
            agent_client_protocol::schema::v1::RequestPermissionResponse,
            agent_client_protocol::Error,
        >,
    > {
        use agent_client_protocol::schema::v1::{
            RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
        };
        Box::pin(async move {
            fs::remove_file(&self.path)
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            std::os::unix::fs::symlink(&self.outside, &self.path)
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    crate::session::PERMISSION_ALLOW_ONCE_OPTION_ID,
                )),
            ))
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn file_roots_reject_symlink_replacement_during_approval()
-> Result<(), Box<dyn std::error::Error>> {
    for delegated in [false, true] {
        for name in ["write_file", "edit_file"] {
            let workspace = Workspace::new(delegated)?;
            workspace
                .store
                .set_mode(&workspace.context.session_id, SessionBehavior::Ask)?;
            let permission = SwapOnApproval {
                path: workspace.context.cwd.join("inside.txt"),
                outside: workspace.outside.join("secret.txt"),
            };
            let read = CountingReadTextFileRequester::new();
            let write = RecordingWriteTextFileRequester::new();
            let call = ChatToolCall::new("swap", name, serde_json::json!({
                "path":"inside.txt", "content":"overwritten", "old_text": if delegated {"client content"} else {"inside"}, "new_text":"overwritten"
            }).to_string());
            let token = CancellationToken::new();
            let result = if name == "write_file" {
                write_file_tool_execution(
                    &workspace.store,
                    &call,
                    &workspace.context,
                    Some(&read),
                    Some(&write),
                    Some(&permission),
                    &token,
                )
                .await
            } else {
                edit_file_tool_execution(
                    &workspace.store,
                    &call,
                    &workspace.context,
                    Some(&read),
                    Some(&write),
                    Some(&permission),
                    &token,
                )
                .await
            };
            assert!(!result.success, "{name} accepted swapped path");
            assert_eq!(fs::read_to_string(&permission.outside)?, "private sentinel");
            assert!(
                write
                    .requests()
                    .lock()
                    .map_err(|e| e.to_string())?
                    .is_empty()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn file_roots_allow_in_root_symlinks_but_reject_loops()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    for (target, alias, text) in [
        (
            workspace.context.cwd.join("inside.txt"),
            "local-link",
            "inside",
        ),
        (workspace.extra.join("extra.txt"), "extra-link", "extra"),
    ] {
        std::os::unix::fs::symlink(target, workspace.context.cwd.join(alias))?;
        let call = ChatToolCall::new(
            "link",
            "read_file",
            serde_json::json!({"path":alias}).to_string(),
        );
        let result = read_file_tool_execution(&call, &workspace.context, None).await;
        assert!(result.success, "{}", result.content);
        assert_eq!(result.content, text);
    }
    std::os::unix::fs::symlink("loop", workspace.context.cwd.join("loop"))?;
    assert!(ConfinedPath::resolve(&workspace.context.cwd, &[], Path::new("loop")).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_roots_pin_directory_access_across_parent_replacement()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    let directory = workspace.context.cwd.join("nested");
    fs::create_dir(&directory)?;
    fs::write(directory.join("secret.txt"), "allowed")?;
    let path = ConfinedPath::resolve(&workspace.context.cwd, &[], Path::new("nested/secret.txt"))?;
    fs::rename(&directory, workspace.context.cwd.join("original"))?;
    std::os::unix::fs::symlink(&workspace.outside, &directory)?;
    assert!(path.read_to_string().is_err());
    assert!(path.write("overwritten").is_err());
    assert!(path.path_for_client().is_err());
    assert_eq!(
        fs::read_to_string(workspace.outside.join("secret.txt"))?,
        "private sentinel"
    );
    Ok(())
}

#[test]
fn search_roots_do_not_consume_parent_ignore_files() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    fs::write(workspace.root.join(".ignore"), "inside.txt\n")?;
    for (name, pattern) in [("glob", "*.txt"), ("grep", "inside")] {
        let call = ChatToolCall::new(
            "search",
            name,
            serde_json::json!({"pattern":pattern}).to_string(),
        );
        let result = if name == "glob" {
            glob_tool_execution(&call, &workspace.context, &CancellationToken::new())
        } else {
            grep_tool_execution(&call, &workspace.context, &CancellationToken::new())
        };
        assert!(result.success, "{}", result.content);
        assert!(
            result.content.contains("inside.txt"),
            "{name} read a parent ignore file: {}",
            result.content
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn search_roots_reject_external_ignore_symlinks() -> Result<(), Box<dyn std::error::Error>> {
    for ignore_name in [".gitignore", ".ignore"] {
        let workspace = Workspace::new(false)?;
        fs::write(workspace.outside.join("rules"), "inside.txt\n")?;
        std::os::unix::fs::symlink(
            workspace.outside.join("rules"),
            workspace.context.cwd.join(ignore_name),
        )?;
        for (name, pattern) in [("glob", "*.txt"), ("grep", "inside")] {
            let call = ChatToolCall::new(
                "search",
                name,
                serde_json::json!({"pattern":pattern}).to_string(),
            );
            let result = if name == "glob" {
                glob_tool_execution(&call, &workspace.context, &CancellationToken::new())
            } else {
                grep_tool_execution(&call, &workspace.context, &CancellationToken::new())
            };
            assert!(!result.success, "{name} accepted external {ignore_name}");
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn search_roots_skip_external_symlinks_and_additional_directories()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    std::os::unix::fs::symlink(&workspace.outside, workspace.context.cwd.join("escape"))?;
    std::os::unix::fs::symlink(
        workspace.outside.join("secret.txt"),
        workspace.context.cwd.join("leak.txt"),
    )?;
    for (name, pattern) in [("glob", "**/*.txt"), ("grep", "inside|private|extra")] {
        let call = ChatToolCall::new(
            "search",
            name,
            serde_json::json!({"pattern":pattern}).to_string(),
        );
        let result = if name == "glob" {
            glob_tool_execution(&call, &workspace.context, &CancellationToken::new())
        } else {
            grep_tool_execution(&call, &workspace.context, &CancellationToken::new())
        };
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("inside.txt"));
        for forbidden in ["secret", "private", "leak", "extra"] {
            assert!(
                !result.content.contains(forbidden),
                "{name} leaked {forbidden}: {}",
                result.content
            );
        }
    }
    Ok(())
}

#[test]
fn search_roots_preserve_nested_ignore_negation_and_hidden_exclusion()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    let cwd = &workspace.context.cwd;
    for directory in ["nested", "ignored", ".hidden"] {
        fs::create_dir(cwd.join(directory))?;
    }
    for file in [
        "nested/keep.tmp",
        "nested/drop.tmp",
        "nested/drop.skip",
        "nested/keep.txt",
        "ignored/secret.txt",
        ".hidden/secret.txt",
        ".secret.txt",
    ] {
        fs::write(cwd.join(file), "match")?;
    }
    fs::write(cwd.join(".gitignore"), "*.tmp\nignored/\n")?;
    fs::write(cwd.join("nested/.gitignore"), "!keep.tmp\n!drop.skip\n")?;
    fs::write(cwd.join(".ignore"), "*.skip\n")?;
    for (name, pattern) in [("glob", "**/*"), ("grep", "match")] {
        let call = ChatToolCall::new(
            "search",
            name,
            serde_json::json!({"pattern":pattern}).to_string(),
        );
        let result = if name == "glob" {
            glob_tool_execution(&call, &workspace.context, &CancellationToken::new())
        } else {
            grep_tool_execution(&call, &workspace.context, &CancellationToken::new())
        };
        assert!(result.success, "{}", result.content);
        for allowed in ["nested/keep.tmp", "nested/keep.txt"] {
            assert!(
                result.content.contains(allowed),
                "{name} missed {allowed}: {}",
                result.content
            );
        }
        for denied in ["drop", "secret", ".ignore", ".gitignore"] {
            assert!(
                !result.content.contains(denied),
                "{name} included {denied}: {}",
                result.content
            );
        }
    }
    Ok(())
}

#[test]
fn search_roots_have_stable_capped_order_and_cooperative_cancellation()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read as _;
    let workspace = Workspace::new(false)?;
    for index in (0..210).rev() {
        fs::write(
            workspace.context.cwd.join(format!("{index:03}.txt")),
            "match",
        )?;
    }
    for name in ["glob", "grep"] {
        let pattern = if name == "glob" { "*.txt" } else { "match" };
        let call = ChatToolCall::new(
            "search",
            name,
            serde_json::json!({"pattern":pattern}).to_string(),
        );
        let run = |token: &CancellationToken| {
            if name == "glob" {
                glob_tool_execution(&call, &workspace.context, token)
            } else {
                grep_tool_execution(&call, &workspace.context, token)
            }
        };
        let first = run(&CancellationToken::new());
        assert!(first.success, "{}", first.content);
        assert_eq!(
            first.raw_output.get("truncated"),
            Some(&serde_json::json!(true))
        );
        assert!(first.content.starts_with("000.txt"));
        assert!(!first.content.contains("200.txt"));
        assert_eq!(first.content, run(&CancellationToken::new()).content);
        let token = CancellationToken::new();
        token.cancel();
        assert!(!run(&token).success);
    }
    let root = ConfinedPath::resolve(&workspace.context.cwd, &[], Path::new("."))?;
    let token = CancellationToken::new();
    let mut visited = 0;
    let result = walk_files(&root, &token, |_, _| {
        visited += 1;
        token.cancel();
        Ok(true)
    });
    assert!(result.is_err());
    assert_eq!(visited, 1);

    // The grep reader checks cancellation even when no lines match.
    let directory = root.directory()?;
    let mut reader = SearchReader {
        file: directory.open("inside.txt")?,
        cancellation: &token,
    };
    let mut buffer = [0_u8; 8];
    assert!(reader.read(&mut buffer).is_err());
    assert_eq!(buffer, [0; 8]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn search_roots_pin_traversal_across_root_replacement() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read as _;
    let workspace = Workspace::new(false)?;
    let root = ConfinedPath::resolve(&workspace.context.cwd, &[], Path::new("."))?;
    fs::write(workspace.context.cwd.join("a-trigger.txt"), "trigger")?;
    let mut output = String::new();
    walk_files(&root, &CancellationToken::new(), |path, entry| {
        if path == Path::new("a-trigger.txt") {
            fs::rename(&workspace.context.cwd, workspace.root.join("original"))
                .map_err(|error| error.to_string())?;
            std::os::unix::fs::symlink(&workspace.outside, &workspace.context.cwd)
                .map_err(|error| error.to_string())?;
        }
        entry
            .open()
            .map_err(|error| error.to_string())?
            .read_to_string(&mut output)
            .map_err(|error| error.to_string())?;
        Ok(true)
    })?;
    assert_eq!(output, "triggerinside");
    Ok(())
}

#[cfg(unix)]
#[test]
fn search_roots_reject_replacement_between_enumeration_and_open()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    let root = ConfinedPath::resolve(&workspace.context.cwd, &[], Path::new("."))?;
    let mut checked = false;
    walk_files(&root, &CancellationToken::new(), |path, entry| {
        fs::remove_file(workspace.context.cwd.join(path)).map_err(|error| error.to_string())?;
        std::os::unix::fs::symlink(
            workspace.outside.join("secret.txt"),
            workspace.context.cwd.join(path),
        )
        .map_err(|error| error.to_string())?;
        assert!(entry.open().is_err());
        checked = true;
        Ok(true)
    })?;
    assert!(checked);
    Ok(())
}

#[cfg(unix)]
#[test_log::test(tokio::test)]
async fn file_roots_reject_external_and_dangling_symlinks_in_both_routes()
-> Result<(), Box<dyn std::error::Error>> {
    for delegated in [false, true] {
        let workspace = Workspace::new(delegated)?;
        std::os::unix::fs::symlink(&workspace.outside, workspace.context.cwd.join("escape"))?;
        std::os::unix::fs::symlink(
            workspace.outside.join("new.txt"),
            workspace.context.cwd.join("dangling"),
        )?;
        let read = CountingReadTextFileRequester::new();
        let write = RecordingWriteTextFileRequester::new();
        let permission = FakePermissionRequester::new(vec![]);
        for path in ["escape/secret.txt", "escape/new.txt", "dangling"] {
            let call = ChatToolCall::new(
                "write",
                "write_file",
                serde_json::json!({"path":path,"content":"escaped"}).to_string(),
            );
            let result = write_file_tool_execution(
                &workspace.store,
                &call,
                &workspace.context,
                Some(&read),
                Some(&write),
                Some(&permission),
                &CancellationToken::new(),
            )
            .await;
            assert!(!result.success, "accepted {path}");
        }
        assert_eq!(
            fs::read_to_string(workspace.outside.join("secret.txt"))?,
            "private sentinel"
        );
        assert!(!workspace.outside.join("new.txt").exists());
        assert_eq!(*read.calls().lock().map_err(|e| e.to_string())?, 0);
        assert!(
            write
                .requests()
                .lock()
                .map_err(|e| e.to_string())?
                .is_empty()
        );
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn file_roots_preserve_cwd_additional_roots_and_new_files()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::new(false)?;
    for (path, text) in [
        (PathBuf::from("inside.txt"), "inside"),
        (PathBuf::from("extra.txt"), "extra"),
        (workspace.extra.join("extra.txt"), "extra"),
    ] {
        let call = ChatToolCall::new(
            "read",
            "read_file",
            serde_json::json!({"path":path}).to_string(),
        );
        let result = read_file_tool_execution(&call, &workspace.context, None).await;
        assert!(result.success, "{}", result.content);
        assert_eq!(result.content, text);
    }
    let permission = FakePermissionRequester::new(vec![]);
    for path in [
        workspace.context.cwd.join("new.txt"),
        workspace.extra.join("new.txt"),
    ] {
        let call = ChatToolCall::new(
            "write",
            "write_file",
            serde_json::json!({"path":path,"content":"new"}).to_string(),
        );
        let result = write_file_tool_execution(
            &workspace.store,
            &call,
            &workspace.context,
            None,
            None,
            Some(&permission),
            &CancellationToken::new(),
        )
        .await;
        assert!(result.success, "{}", result.content);
        assert_eq!(fs::read_to_string(path)?, "new");
    }
    Ok(())
}
