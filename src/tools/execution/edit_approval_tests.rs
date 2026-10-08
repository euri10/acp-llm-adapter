//! An approval authorizes an edit only while its input document is unchanged.

use super::*;
use crate::acp::handle_new_session_request;
use crate::test_utils::RecordingWriteTextFileRequester;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, FileSystemCapabilities, NewSessionRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const ORIGINAL: &str = "alpha\nuser original\n";

struct ChangeOnApproval {
    local_path: Option<PathBuf>,
    current: Mutex<Option<&'static str>>,
    replacement: Option<&'static str>,
    approved: AtomicBool,
    cancel_on_refresh: Option<CancellationToken>,
}

impl ReadTextFileRequester for ChangeOnApproval {
    fn read_text_file(
        &self,
        _request: ReadTextFileRequest,
    ) -> BoxFuture<'_, Result<ReadTextFileResponse, agent_client_protocol::Error>> {
        Box::pin(async move {
            if self.approved.load(Ordering::SeqCst)
                && let Some(token) = &self.cancel_on_refresh
            {
                token.cancel();
                return std::future::pending().await;
            }
            self.current
                .lock()
                .map_err(agent_client_protocol::Error::into_internal_error)?
                .map(ReadTextFileResponse::new)
                .ok_or_else(|| agent_client_protocol::Error::internal_error().data("file deleted"))
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
                std::fs::remove_file(path)
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                if let Some(replacement) = self.replacement {
                    std::fs::write(path, replacement)
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                }
            }
            *self
                .current
                .lock()
                .map_err(agent_client_protocol::Error::into_internal_error)? = self.replacement;
            self.approved.store(true, Ordering::SeqCst);
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    crate::session::PERMISSION_ALLOW_ONCE_OPTION_ID,
                )),
            ))
        })
    }
}

#[test_log::test(tokio::test)]
async fn edit_file_preserves_changes_and_deletions_during_approval()
-> Result<(), Box<dyn std::error::Error>> {
    for delegated in [false, true] {
        for (replacement, cancelled) in [
            (Some("alpha\nUSER CHANGED THIS WHILE APPROVING\n"), false),
            (Some("old match removed\n"), false),
            (Some("alpha alpha\nambiguous match\n"), false),
            (None, false),
            (Some(ORIGINAL), true),
        ] {
            if cancelled && !delegated {
                continue;
            }
            let root =
                std::env::temp_dir().join(format!("acp-edit-approval-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root)?;
            let path = root.join("note.txt");
            std::fs::write(&path, ORIGINAL)?;
            let store = crate::test_store();
            let session = handle_new_session_request(&store, &NewSessionRequest::new(&root))?;
            let context = ToolContext {
                session_id: session.session_id.0.to_string(),
                cwd: root.clone(),
                additional_directories: Vec::new(),
                client_capabilities: Some(
                    (ClientCapabilities::new().fs(FileSystemCapabilities::new()
                        .read_text_file(delegated)
                        .write_text_file(delegated)))
                    .into(),
                ),
            };
            let token = CancellationToken::new();
            let editor = ChangeOnApproval {
                local_path: (!delegated).then(|| path.clone()),
                current: Mutex::new(Some(ORIGINAL)),
                replacement,
                approved: AtomicBool::new(false),
                cancel_on_refresh: cancelled.then(|| token.clone()),
            };
            let writer = RecordingWriteTextFileRequester::new();
            let call = ChatToolCall::new(
                "edit",
                "edit_file",
                serde_json::json!({
                    "path":"note.txt", "old_text":"alpha", "new_text":"beta"
                })
                .to_string(),
            );
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                edit_file_tool_execution(
                    &store,
                    &call,
                    &context,
                    Some(&editor),
                    Some(&writer),
                    Some(&editor),
                    &token,
                ),
            )
            .await?;

            assert!(
                !result.success,
                "overwrote {replacement:?}; delegated={delegated}"
            );
            if cancelled {
                assert!(result.content.contains("cancelled"), "{}", result.content);
            } else if replacement.is_some() {
                assert!(
                    result.content.contains("changed while awaiting approval"),
                    "{}",
                    result.content
                );
            }
            assert!(result.edit.is_none());
            assert!(
                writer
                    .requests()
                    .lock()
                    .map_err(|error| error.to_string())?
                    .is_empty()
            );
            assert_eq!(
                *editor.current.lock().map_err(|error| error.to_string())?,
                replacement
            );
            if delegated {
                assert_eq!(std::fs::read_to_string(&path)?, ORIGINAL);
            } else if let Some(replacement) = replacement {
                assert_eq!(std::fs::read_to_string(&path)?, replacement);
            } else {
                assert!(!path.exists(), "deleted file was recreated");
            }
            std::fs::remove_dir_all(root)?;
        }
    }
    Ok(())
}
