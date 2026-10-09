//! A file-edit permission request previews the exact change it authorizes.

use super::*;
use crate::acp::handle_new_session_request;
use crate::test_utils::{FakePermissionRequester, RecordingWriteTextFileRequester};
use agent_client_protocol::schema::v1::{
    ClientCapabilities, Diff, FileSystemCapabilities, NewSessionRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, ToolCallContent,
};
use std::sync::Mutex;

fn allow_once() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(crate::session::PERMISSION_ALLOW_ONCE_OPTION_ID),
    ))
}

fn temp_root(label: &str) -> Result<PathBuf, std::io::Error> {
    let root = std::env::temp_dir().join(format!("acp-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn context(
    store: &SessionStore,
    root: &Path,
    fs: Option<(bool, bool)>,
) -> Result<ToolContext, agent_client_protocol::Error> {
    let session = handle_new_session_request(store, &NewSessionRequest::new(root))?;
    Ok(ToolContext {
        session_id: session.session_id.0.to_string(),
        cwd: root.to_path_buf(),
        additional_directories: Vec::new(),
        client_capabilities: fs.map(|(read, write)| {
            ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(read)
                    .write_text_file(write))
                .into()
        }),
    })
}

fn only_request_content(
    requester: &FakePermissionRequester,
) -> Result<Option<Vec<ToolCallContent>>, String> {
    let requests = requester.requests();
    let requests = requests.lock().map_err(|error| error.to_string())?;
    match requests.as_slice() {
        [request] => Ok(request.tool_call.fields.content.clone()),
        other => Err(format!(
            "expected one permission request, got {}",
            other.len()
        )),
    }
}

fn diff(path: PathBuf, old_text: Option<&str>, new_text: &str) -> Vec<ToolCallContent> {
    vec![ToolCallContent::Diff(
        Diff::new(path, new_text).old_text(old_text.map(str::to_owned)),
    )]
}

#[test_log::test(tokio::test)]
async fn edit_file_permission_request_previews_the_whole_file()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root("edit-preview")?;
    let path = root.join("note.txt");
    std::fs::write(&path, "alpha beta gamma\n")?;
    let store = crate::test_store();
    let context = context(&store, &root, None)?;
    let permission = FakePermissionRequester::new(vec![allow_once()]);
    let call = ChatToolCall::new(
        "edit",
        "edit_file",
        serde_json::json!({"path":"note.txt", "old_text":"beta", "new_text":"delta"}).to_string(),
    );

    let result = edit_file_tool_execution(
        &store,
        &call,
        &context,
        None,
        None,
        Some(&permission),
        &CancellationToken::new(),
    )
    .await;

    assert!(result.success, "{}", result.content);
    assert_eq!(
        only_request_content(&permission)?,
        Some(diff(
            path.clone(),
            Some("alpha beta gamma\n"),
            "alpha delta gamma\n"
        ))
    );
    assert_eq!(std::fs::read_to_string(&path)?, "alpha delta gamma\n");
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn write_file_permission_request_previews_replacement_and_creation()
-> Result<(), Box<dyn std::error::Error>> {
    for existing in [Some("old content\n"), None] {
        let root = temp_root("write-preview")?;
        let path = root.join("note.txt");
        if let Some(existing) = existing {
            std::fs::write(&path, existing)?;
        }
        let store = crate::test_store();
        let context = context(&store, &root, None)?;
        let permission = FakePermissionRequester::new(vec![allow_once()]);
        let call = ChatToolCall::new(
            "write",
            "write_file",
            serde_json::json!({"path":"note.txt", "content":"new content\n"}).to_string(),
        );

        let result = write_file_tool_execution(
            &store,
            &call,
            &context,
            None,
            None,
            Some(&permission),
            &CancellationToken::new(),
        )
        .await;

        assert!(result.success, "{}", result.content);
        assert_eq!(
            only_request_content(&permission)?,
            Some(diff(path.clone(), existing, "new content\n")),
            "existing={existing:?}"
        );
        assert_eq!(std::fs::read_to_string(&path)?, "new content\n");
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn write_file_without_client_reads_claims_no_old_contents()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root("write-unreadable")?;
    let store = crate::test_store();
    let context = context(&store, &root, Some((false, true)))?;
    let permission = FakePermissionRequester::new(vec![allow_once()]);
    let writer = RecordingWriteTextFileRequester::new();
    let call = ChatToolCall::new(
        "write",
        "write_file",
        serde_json::json!({"path":"note.txt", "content":"new content\n"}).to_string(),
    );

    let result = write_file_tool_execution(
        &store,
        &call,
        &context,
        None,
        Some(&writer),
        Some(&permission),
        &CancellationToken::new(),
    )
    .await;

    // The editor may hold an existing document the adapter cannot read: a
    // creation diff, before or after the write, would misstate the change.
    assert!(result.success, "{}", result.content);
    assert_eq!(only_request_content(&permission)?, None);
    assert_eq!(result.edit, None);
    assert_eq!(
        writer
            .requests()
            .lock()
            .map_err(|error| error.to_string())?
            .len(),
        1
    );
    std::fs::remove_dir_all(root)?;
    Ok(())
}

const ORIGINAL: &str = "user original\n";

/// Plays the editor: serves reads of one document and changes it on approval.
struct ChangeOnApproval {
    local_path: Option<PathBuf>,
    current: Mutex<Option<&'static str>>,
    during_approval: Option<&'static str>,
}

impl ReadTextFileRequester for ChangeOnApproval {
    fn read_text_file(
        &self,
        _request: ReadTextFileRequest,
    ) -> BoxFuture<'_, Result<ReadTextFileResponse, agent_client_protocol::Error>> {
        Box::pin(async move {
            self.current
                .lock()
                .map_err(agent_client_protocol::Error::into_internal_error)?
                .map(ReadTextFileResponse::new)
                .ok_or_else(|| agent_client_protocol::Error::resource_not_found(None))
        })
    }
}

impl PermissionRequester for ChangeOnApproval {
    fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> BoxFuture<'_, Result<RequestPermissionResponse, agent_client_protocol::Error>> {
        Box::pin(async move {
            if let Some(path) = &self.local_path {
                match self.during_approval {
                    Some(text) => std::fs::write(path, text),
                    None => std::fs::remove_file(path),
                }
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            }
            *self
                .current
                .lock()
                .map_err(agent_client_protocol::Error::into_internal_error)? = self.during_approval;
            Ok(allow_once())
        })
    }
}

#[test_log::test(tokio::test)]
async fn write_file_refuses_documents_changed_during_approval()
-> Result<(), Box<dyn std::error::Error>> {
    for delegated in [false, true] {
        for (initial, during_approval, expect_write) in [
            (Some(ORIGINAL), Some("user changed this\n"), false),
            (Some(ORIGINAL), None, false),
            (None, Some("user created this\n"), false),
            (Some(ORIGINAL), Some(ORIGINAL), true),
        ] {
            let root = temp_root("write-approval")?;
            let path = root.join("note.txt");
            if let Some(initial) = initial {
                std::fs::write(&path, initial)?;
            }
            let store = crate::test_store();
            let context = context(&store, &root, Some((delegated, delegated)))?;
            let editor = ChangeOnApproval {
                local_path: (!delegated).then(|| path.clone()),
                current: Mutex::new(initial),
                during_approval,
            };
            let writer = RecordingWriteTextFileRequester::new();
            let call = ChatToolCall::new(
                "write",
                "write_file",
                serde_json::json!({"path":"note.txt", "content":"agent content\n"}).to_string(),
            );

            let result = write_file_tool_execution(
                &store,
                &call,
                &context,
                Some(&editor),
                Some(&writer),
                Some(&editor),
                &CancellationToken::new(),
            )
            .await;

            let case = format!("delegated={delegated} {initial:?} -> {during_approval:?}");
            assert_eq!(result.success, expect_write, "{case}: {}", result.content);
            let client_writes = writer
                .requests()
                .lock()
                .map_err(|error| error.to_string())?
                .len();
            if expect_write {
                if delegated {
                    assert_eq!(client_writes, 1, "{case}");
                } else {
                    assert_eq!(std::fs::read_to_string(&path)?, "agent content\n", "{case}");
                }
            } else {
                assert!(
                    result.content.contains("changed while awaiting approval"),
                    "{case}: {}",
                    result.content
                );
                assert_eq!(result.edit, None, "{case}");
                assert_eq!(client_writes, 0, "{case}");
                if !delegated {
                    match during_approval {
                        Some(text) => assert_eq!(std::fs::read_to_string(&path)?, text, "{case}"),
                        None => assert!(!path.exists(), "{case}: deleted file was recreated"),
                    }
                }
            }
            std::fs::remove_dir_all(root)?;
        }
    }
    Ok(())
}
