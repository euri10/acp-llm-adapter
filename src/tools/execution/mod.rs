//! Tool definitions, execution, and helper utilities.

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::{ToolCall as ChatToolCall, ToolDefinition};
use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    ReadTextFileRequest, TerminalOutputRequest, WaitForTerminalExitRequest, WriteTextFileRequest,
};
use globset::{Glob, GlobSetBuilder};
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::{BinaryDetection, SearcherBuilder};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::filesystem::ConfinedPath;
use super::registry::{
    AdapterToolRegistry, ToolContext, ToolEdit, ToolExecution, ToolKind, ToolRegistry,
};
use super::search::{SearchReader, walk_files};
use crate::{
    PermissionDecision, PermissionRequester, ReadTextFileRequester, SessionBehavior, SessionStore,
    TerminalRequester, ToolProgressReporter, WriteTextFileRequester, request_tool_permission,
};
use futures_util::future::BoxFuture;

const TOOL_OUTPUT_LIMIT: usize = 200;
const TOOL_OUTPUT_LIMIT_U32: u32 = 200;
const COMMAND_OUTPUT_LIMIT: usize = 20_000;
/// Character ceiling for file-content tool results (`read_file`, `grep`).
///
/// The line/entry caps ([`TOOL_OUTPUT_LIMIT`]) already bound normal files. This
/// is a safety net for pathological wide-line inputs (e.g. minified assets)
/// whose single lines would otherwise add multi-megabyte tool results to the
/// conversation history and eventually 400 the LLM API.
const FILE_OUTPUT_LIMIT: usize = 65_536;

#[derive(Debug, Deserialize)]
struct ReadFileArguments {
    path: PathBuf,
    line: Option<u32>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ListDirArguments {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct GlobArguments {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct GrepArguments {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct WriteFileArguments {
    path: PathBuf,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileArguments {
    path: PathBuf,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct RunCommandArguments {
    command: String,
}

pub(crate) fn read_file_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "read_file",
        "Read a text file, using the client's file system when available.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "line": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1 },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn list_dir_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "list_dir",
        "List entries in a directory.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn glob_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "glob",
        "Find paths matching a glob pattern.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn grep_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "grep",
        "Search files for a regular expression.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn write_file_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "write_file",
        "Write UTF-8 text to a file, creating or replacing the file.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn edit_file_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "edit_file",
        "Replace one exact UTF-8 text span in an existing file.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" },
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn run_command_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "run_command",
        "Run a shell command in the session working directory.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn update_plan_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "update_plan",
        "Update the current plan with structured steps.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "priority": {
                                "type": "string",
                                "enum": ["high", "medium", "low"],
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                            },
                        },
                        "required": ["content", "priority", "status"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["entries"],
            "additionalProperties": false,
        }),
    )
}

pub(crate) fn exit_plan_mode_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "exit_plan_mode",
        "Exit Plan mode by choosing another session mode.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
}

#[derive(Debug, Deserialize)]
struct UpdatePlanArguments {
    entries: Vec<UpdatePlanEntryArguments>,
}

#[derive(Debug, Deserialize)]
struct UpdatePlanEntryArguments {
    content: String,
    priority: PlanEntryPriority,
    status: PlanEntryStatus,
}

pub(crate) fn update_plan_tool_execution(call: &ChatToolCall) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<UpdatePlanArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid update_plan arguments: {error}"));
        }
    };

    let entries = parsed_arguments
        .entries
        .into_iter()
        .map(|entry| PlanEntry::new(entry.content, entry.priority, entry.status))
        .collect::<Vec<_>>();
    let plan = Plan::new(entries);

    let entry_count = plan.entries.len();
    match serde_json::to_value(&plan) {
        Ok(raw_output) => ToolExecution {
            content: format!("updated plan with {entry_count} entries"),
            raw_output,
            success: true,
            edit: None,
        },
        Err(error) => {
            ToolExecution::failed(format!("failed to serialize update_plan result: {error}"))
        }
    }
}

pub(crate) fn exit_plan_mode_tool_execution(
    store: &SessionStore,
    call: &ChatToolCall,
    context: &ToolContext,
) -> ToolExecution {
    let _arguments = match serde_json::from_str::<serde_json::Value>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid exit_plan_mode arguments: {error}"));
        }
    };

    match store.session_behavior(&context.session_id) {
        Ok(SessionBehavior::Plan) => {}
        Ok(_) => {
            return ToolExecution::failed("exit_plan_mode is only available while in Plan mode");
        }
        Err(error) => {
            return ToolExecution::failed(format!("failed to exit plan mode: {error}"));
        }
    }

    if let Err(error) = store.set_mode(&context.session_id, SessionBehavior::AcceptEdits) {
        return ToolExecution::failed(format!("failed to exit plan mode: {error}"));
    }

    ToolExecution {
        content: format!("switched to {} mode", SessionBehavior::AcceptEdits.name()),
        raw_output: serde_json::json!({
            "mode_id": SessionBehavior::AcceptEdits.mode_id(),
            "mode_name": SessionBehavior::AcceptEdits.name(),
        }),
        success: true,
        edit: None,
    }
}

pub(crate) async fn read_file_tool_execution(
    call: &ChatToolCall,
    context: &ToolContext,
    connection: Option<&dyn ReadTextFileRequester>,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<ReadFileArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid read_file arguments: {error}"));
        }
    };

    let resolved_path = match resolve_tool_path(context, &parsed_arguments.path).await {
        Ok(path) => path,
        Err(error) => return ToolExecution::failed(error),
    };
    let start_line = parsed_arguments.line.unwrap_or(1);
    let requested_limit = parsed_arguments.limit.unwrap_or(TOOL_OUTPUT_LIMIT_U32);
    let limit = requested_limit.min(TOOL_OUTPUT_LIMIT_U32);

    if start_line == 0 {
        return ToolExecution::failed("read_file line must be at least 1");
    }

    if requested_limit == 0 {
        return ToolExecution::failed("read_file limit must be at least 1");
    }

    let file_result = if context
        .client_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.read_text_file)
    {
        match connection {
            Some(connection) => {
                if local_file_is_non_utf8(&resolved_path).await {
                    return ToolExecution::failed(non_utf8_file_message(&resolved_path.path));
                }

                read_file_from_client(
                    connection,
                    &context.session_id,
                    &resolved_path,
                    start_line,
                    limit,
                )
                .await
            }
            None => Err("read_file needs a client connection for fs/read_text_file".to_owned()),
        }
    } else {
        read_file_from_local(&resolved_path, start_line, limit).await
    };

    match file_result {
        Ok(file_slice) => ToolExecution {
            content: truncate_tool_output(&file_slice, FILE_OUTPUT_LIMIT).0,
            raw_output: serde_json::json!({
                "path": resolved_path.path,
                "line": start_line,
                "limit": limit,
                "source": if context
                    .client_capabilities
                    .as_ref()
                    .is_some_and(|capabilities| capabilities.read_text_file)
                {
                    "client"
                } else {
                    "local"
                },
            }),
            success: true,
            edit: None,
        },
        Err(error) => ToolExecution::failed(error),
    }
}

pub(crate) async fn write_file_tool_execution(
    store: &SessionStore,
    call: &ChatToolCall,
    context: &ToolContext,
    read_connection: Option<&dyn ReadTextFileRequester>,
    write_connection: Option<&dyn WriteTextFileRequester>,
    permission_requester: Option<&dyn PermissionRequester>,
    cancellation: &CancellationToken,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<WriteFileArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid write_file arguments: {error}"));
        }
    };

    let resolved_path = match resolve_tool_path(context, &parsed_arguments.path).await {
        Ok(path) => path,
        Err(error) => return ToolExecution::failed(error),
    };

    if let Err(error) = require_tool_permission(
        store,
        context,
        call,
        ToolKind::Edit,
        permission_requester,
        cancellation,
    )
    .await
    {
        return ToolExecution::failed(error);
    }

    let use_client_write = context
        .client_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.write_text_file);
    let old_text = match tokio::select! {
        biased;
        () = cancellation.cancelled() => return ToolExecution::failed("write_file cancelled"),
        result = read_existing_text(context, &resolved_path, read_connection, use_client_write) => result,
    } {
        Ok(text) => text,
        Err(error) => return ToolExecution::failed(error),
    };
    if cancellation.is_cancelled() {
        return ToolExecution::failed("write_file cancelled");
    }
    let write_result = if use_client_write {
        match write_connection {
            Some(connection) => {
                write_file_to_client(
                    connection,
                    store,
                    &context.session_id,
                    &resolved_path,
                    &parsed_arguments.content,
                    cancellation,
                )
                .await
            }
            None => Err("write_file needs a client connection for fs/write_text_file".to_owned()),
        }
    } else {
        write_file_to_local(
            store,
            &context.session_id,
            &resolved_path,
            &parsed_arguments.content,
            cancellation,
        )
        .await
    };

    match write_result {
        Ok(()) => {
            let byte_count = parsed_arguments.content.len();
            ToolExecution {
                content: format!(
                    "wrote {byte_count} bytes to {}",
                    resolved_path.path.display()
                ),
                raw_output: serde_json::json!({
                    "path": resolved_path.path,
                    "bytes": byte_count,
                    "source": if use_client_write { "client" } else { "local" },
                }),
                success: true,
                edit: Some(ToolEdit {
                    path: resolved_path.path,
                    old_text,
                    new_text: parsed_arguments.content,
                    line: 1,
                }),
            }
        }
        Err(error) => ToolExecution::failed(error),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the read, approval, cancellation and write stages of one edit transaction together."
)]
pub(crate) async fn edit_file_tool_execution(
    store: &SessionStore,
    call: &ChatToolCall,
    context: &ToolContext,
    read_connection: Option<&dyn ReadTextFileRequester>,
    write_connection: Option<&dyn WriteTextFileRequester>,
    permission_requester: Option<&dyn PermissionRequester>,
    cancellation: &CancellationToken,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<EditFileArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid edit_file arguments: {error}"));
        }
    };

    if parsed_arguments.old_text.is_empty() {
        return ToolExecution::failed("edit_file old_text must not be empty");
    }

    let resolved_path = match resolve_tool_path(context, &parsed_arguments.path).await {
        Ok(path) => path,
        Err(error) => return ToolExecution::failed(error),
    };
    let use_client_read = context
        .client_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.read_text_file);
    let original =
        match read_edit_source(context, &resolved_path, read_connection, cancellation).await {
            Ok(file_text) => file_text,
            Err(error) => return ToolExecution::failed(error),
        };

    let matches = original.matches(&parsed_arguments.old_text).count();
    if matches == 0 {
        return ToolExecution::failed(format!(
            "edit_file could not find old_text in {}",
            resolved_path.path.display()
        ));
    }
    if matches > 1 {
        return ToolExecution::failed(format!(
            "edit_file found old_text {matches} times in {}; provide a unique span",
            resolved_path.path.display()
        ));
    }

    if let Err(error) = require_tool_permission(
        store,
        context,
        call,
        ToolKind::Edit,
        permission_requester,
        cancellation,
    )
    .await
    {
        return ToolExecution::failed(error);
    }

    // Approval can stay open while the user edits or replaces this document.
    // Check the same confined source again before using its original snapshot.
    match read_edit_source(context, &resolved_path, read_connection, cancellation).await {
        Ok(current) if current == original => {}
        Ok(_) => {
            return ToolExecution::failed(format!(
                "edit_file: {} changed while awaiting approval; read it again before retrying",
                resolved_path.path.display()
            ));
        }
        Err(error) => return ToolExecution::failed(error),
    }

    let edit_line = match original.find(&parsed_arguments.old_text) {
        Some(offset) => line_number_for_offset(&original, offset),
        None => 1,
    };
    let updated = original.replacen(&parsed_arguments.old_text, &parsed_arguments.new_text, 1);
    let use_client_write = context
        .client_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.write_text_file);
    let write_result = if use_client_write {
        match write_connection {
            Some(connection) => {
                write_file_to_client(
                    connection,
                    store,
                    &context.session_id,
                    &resolved_path,
                    &updated,
                    cancellation,
                )
                .await
            }
            None => Err("edit_file needs a client connection for fs/write_text_file".to_owned()),
        }
    } else {
        write_file_to_local(
            store,
            &context.session_id,
            &resolved_path,
            &updated,
            cancellation,
        )
        .await
    };

    match write_result {
        Ok(()) => ToolExecution {
            content: format!("edited {}", resolved_path.path.display()),
            raw_output: serde_json::json!({
                "path": resolved_path.path,
                "replacements": 1,
                "read_source": if use_client_read { "client" } else { "local" },
                "write_source": if use_client_write { "client" } else { "local" },
            }),
            success: true,
            edit: Some(ToolEdit {
                path: resolved_path.path,
                old_text: Some(original),
                new_text: updated,
                line: edit_line,
            }),
        },
        Err(error) => ToolExecution::failed(error),
    }
}

/// Owns the command's process group and kills it on drop.
///
/// The kill belongs on the drop path rather than on the cancellation branch
/// alone. A turn also stops when the client disconnects and the serve loop is
/// dropped wholesale, and on that path nothing was cancelled: `kill_on_drop`
/// reaped the shell while everything it had backgrounded kept running after the
/// adapter itself had exited.
///
/// Released when the command exits on its own — whatever it deliberately left
/// behind is then its business, not ours. This guard is only for the case where
/// we stopped waiting first.
#[cfg(unix)]
struct CommandGroup(Option<u32>);

#[cfg(unix)]
impl CommandGroup {
    fn release(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for CommandGroup {
    fn drop(&mut self) {
        kill_command_group(self.0);
    }
}

/// Signal the process group `run_command` put its shell in, so anything the
/// command backgrounded dies with it.
///
/// Best effort by nature: if the command exited between the cancellation and
/// this call the group is already gone, and the resulting error is the expected
/// outcome rather than something to report.
#[cfg(unix)]
fn kill_command_group(process_group: Option<u32>) {
    let Some(raw) = process_group.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
}

/// Run a shell command for the model, delegating to the client's terminal when
/// it advertises one.
///
/// # Cancellation
///
/// Both branches promise the same thing to the caller: a cancelled turn kills
/// the command and returns a failed [`ToolExecution`] reading `run_command
/// cancelled`, never partial output.
///
/// What they can promise about the command's own descendants differs, and the
/// difference is not ours to remove:
///
/// - Terminal branch: the client owns the process, so cancellation issues
///   `terminal/kill` and whatever that client does about descendants applies.
/// - In-process branch on unix: the shell gets its own process group and
///   cancellation signals the group, so work the command backgrounded dies with
///   it (daa-0yi0).
/// - In-process branch elsewhere: no process groups, so only the shell itself is
///   killed and backgrounded descendants survive the turn. Stated rather than
///   silently tolerated; revisit if a non-unix target ever matters.
pub(crate) async fn run_command_tool_execution(
    store: &SessionStore,
    call: &ChatToolCall,
    context: &ToolContext,
    permission_requester: Option<&dyn PermissionRequester>,
    terminal_connection: Option<&dyn TerminalRequester>,
    progress: Option<&dyn ToolProgressReporter>,
    cancellation_token: &CancellationToken,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<RunCommandArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid run_command arguments: {error}"));
        }
    };

    if parsed_arguments.command.trim().is_empty() {
        return ToolExecution::failed("run_command command must not be empty");
    }

    if let Err(error) = require_tool_permission(
        store,
        context,
        call,
        ToolKind::Execute,
        permission_requester,
        cancellation_token,
    )
    .await
    {
        return ToolExecution::failed(error);
    }

    if context
        .client_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.terminal)
    {
        return run_command_via_terminal(
            &context.session_id,
            call.id(),
            &context.cwd,
            &parsed_arguments.command,
            terminal_connection,
            progress,
            cancellation_token,
        )
        .await;
    }

    // tokio's Command rather than a blocking one: a blocking task cannot be
    // cancelled, so `std::process::Command::output()` would hold the turn — and
    // the runtime shutdown behind it — until the command chose to exit.
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-lc")
        .arg(&parsed_arguments.command)
        .current_dir(&context.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cancelling below drops the wait future, which drops the child; this
        // turns that drop into a kill instead of leaving it running unattended.
        .kill_on_drop(true);
    // Own the whole command rather than just the shell. `sh -lc` can background
    // work that a signal to the shell alone would leave running with no owner;
    // its own process group makes that subtree addressable with one signal.
    #[cfg(unix)]
    command.process_group(0);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ToolExecution::failed(format!("failed to run command: {error}")),
    };

    if let Some(progress) = progress {
        progress.report_in_progress(
            &agent_client_protocol::schema::v1::SessionId::new(context.session_id.clone()),
            call.id(),
        );
    }

    // Read before the wait future takes ownership of the child. That future
    // holds the child unreaped until it is dropped, so the kernel cannot
    // recycle this pid onto an unrelated process between here and the signal.
    #[cfg(unix)]
    let mut group = CommandGroup(child.id());

    let output = tokio::select! {
        // Same contract as the terminal branch: kill the command and report the
        // turn as cancelled rather than returning partial output. Dropping
        // `group` on the way out takes the command's descendants with it.
        () = cancellation_token.cancelled() => {
            return ToolExecution::failed("run_command cancelled");
        }
        result = child.wait_with_output() => match result {
            Ok(output) => output,
            Err(error) => {
                return ToolExecution::failed(format!("failed to run command: {error}"));
            }
        },
    };

    // The command ran to completion, so it keeps whatever it chose to leave.
    #[cfg(unix)]
    group.release();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined_output = render_command_output(&stdout, &stderr, output.status.code());
    let (display_output, truncated) = truncate_tool_output(&combined_output, COMMAND_OUTPUT_LIMIT);

    ToolExecution {
        content: display_output,
        raw_output: serde_json::json!({
            "exit_code": output.status.code(),
            "success": output.status.success(),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated,
        }),
        success: output.status.success(),
        edit: None,
    }
}

pub(crate) async fn run_command_via_terminal(
    session_id: &str,
    tool_call_id: &str,
    cwd: &Path,
    command: &str,
    connection: Option<&dyn TerminalRequester>,
    progress: Option<&dyn ToolProgressReporter>,
    cancellation_token: &CancellationToken,
) -> ToolExecution {
    let Some(terminal_requester) = connection else {
        return ToolExecution::failed("terminal support advertised but no connection available");
    };

    let create_request = CreateTerminalRequest::new(session_id.to_string(), command)
        .cwd(Some(cwd.to_path_buf()))
        .output_byte_limit(Some(COMMAND_OUTPUT_LIMIT as u64));
    let create_response = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => return ToolExecution::failed("run_command cancelled"),
        result = terminal_requester.create_terminal(create_request) => match result {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(code = ?error.code, "terminal/create failed");
                return ToolExecution::failed("terminal/create failed");
            }
        },
    };
    let terminal_id = create_response.terminal_id;

    // The client has the command running now; before this the call was queued
    // behind the permission prompt above.
    if let Some(progress) = progress {
        progress.report_in_progress(
            &agent_client_protocol::schema::v1::SessionId::new(session_id),
            tool_call_id,
        );
    }

    let wait_request = WaitForTerminalExitRequest::new(session_id.to_string(), terminal_id.clone());
    let wait_response = tokio::select! {
        biased;
        // Turn cancelled while the command is running: kill it, then release the
        // terminal so the client frees its resources.
        () = cancellation_token.cancelled() => {
            // Cleanup logs failed/unanswered RPCs and attempts both operations.
            let _ = crate::acp::cleanup_terminal(terminal_requester, &agent_client_protocol::schema::v1::SessionId::new(session_id), &terminal_id, true).await;
            return ToolExecution::failed("run_command cancelled");
        }
        result = terminal_requester.wait_for_terminal_exit(wait_request) => match result {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(code = ?error.code, "terminal/wait_for_exit failed");
                // An unsuccessful wait does not prove the command exited.
                let _ = crate::acp::cleanup_terminal(terminal_requester, &agent_client_protocol::schema::v1::SessionId::new(session_id), &terminal_id, true).await;
                return ToolExecution::failed("terminal/wait_for_exit failed");
            }
        },
    };

    let output_request = TerminalOutputRequest::new(session_id.to_string(), terminal_id.clone());
    let output_response = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => {
            // Keep cancellation cleanup consistent even after exit notification.
            let _ = crate::acp::cleanup_terminal(terminal_requester, &agent_client_protocol::schema::v1::SessionId::new(session_id), &terminal_id, true).await;
            return ToolExecution::failed("run_command cancelled");
        }
        result = terminal_requester.terminal_output(output_request) => match result {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(code = ?error.code, "terminal/output failed");
                let _ = crate::acp::cleanup_terminal(terminal_requester, &agent_client_protocol::schema::v1::SessionId::new(session_id), &terminal_id, false).await;
                return ToolExecution::failed("terminal/output failed");
            }
        },
    };

    if crate::acp::cleanup_terminal(
        terminal_requester,
        &agent_client_protocol::schema::v1::SessionId::new(session_id),
        &terminal_id,
        false,
    )
    .await
    .is_err()
    {
        return ToolExecution::failed("terminal/release failed");
    }

    let exit_code = wait_response.exit_status.exit_code;
    let success = exit_code == Some(0);
    let exit_code_i32 = exit_code.and_then(|code| i32::try_from(code).ok());
    let combined_output = render_command_output(&output_response.output, "", exit_code_i32);
    let (display_output, truncated) = truncate_tool_output(&combined_output, COMMAND_OUTPUT_LIMIT);

    ToolExecution {
        content: display_output,
        raw_output: serde_json::json!({
            "exit_code": exit_code,
            "success": success,
            "stdout": output_response.output,
            "stderr": "",
            "truncated": truncated || output_response.truncated,
        }),
        success,
        edit: None,
    }
}

pub(crate) fn list_dir_tool_execution(call: &ChatToolCall, context: &ToolContext) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<ListDirArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolExecution::failed(format!("invalid list_dir arguments: {error}"));
        }
    };

    let resolved_path = match ConfinedPath::resolve(
        &context.cwd,
        &context.additional_directories,
        &parsed_arguments.path,
    ) {
        Ok(path) => path,
        Err(error) => return ToolExecution::failed(error.to_string()),
    };
    let entries = match collect_directory_entries(&resolved_path) {
        Ok(entries) => entries,
        Err(error) => return ToolExecution::failed(error),
    };

    let truncated = entries.len() > TOOL_OUTPUT_LIMIT;
    let entries = entries
        .into_iter()
        .take(TOOL_OUTPUT_LIMIT)
        .collect::<Vec<_>>();
    let output_text = render_tool_lines(&entries, truncated, "entries", TOOL_OUTPUT_LIMIT);

    ToolExecution {
        content: output_text,
        raw_output: serde_json::json!({
            "path": resolved_path.path,
            "entries": entries,
            "truncated": truncated,
        }),
        success: true,
        edit: None,
    }
}

pub(crate) fn glob_tool_execution(
    call: &ChatToolCall,
    context: &ToolContext,
    cancellation: &CancellationToken,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<GlobArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => return ToolExecution::failed(format!("invalid glob arguments: {error}")),
    };

    let matcher = match Glob::new(&parsed_arguments.pattern) {
        Ok(glob) => {
            let mut builder = GlobSetBuilder::new();
            builder.add(glob);
            match builder.build() {
                Ok(set) => set,
                Err(error) => {
                    return ToolExecution::failed(format!("invalid glob pattern: {error}"));
                }
            }
        }
        Err(error) => return ToolExecution::failed(format!("invalid glob pattern: {error}")),
    };

    let root = match ConfinedPath::resolve(&context.cwd, &[], Path::new(".")) {
        Ok(root) => root,
        Err(error) => return ToolExecution::failed(error.to_string()),
    };
    let mut glob_paths = Vec::new();
    if let Err(error) = walk_files(&root, cancellation, |relative_path, _entry| {
        if matcher.is_match(relative_path) || matcher.is_match(context.cwd.join(relative_path)) {
            glob_paths.push(relative_path.display().to_string());
        }
        Ok(true)
    }) {
        return ToolExecution::failed(error);
    }

    glob_paths.sort_unstable();
    let truncated = glob_paths.len() > TOOL_OUTPUT_LIMIT;
    let entries = glob_paths
        .into_iter()
        .take(TOOL_OUTPUT_LIMIT)
        .collect::<Vec<_>>();
    let output_text = render_tool_lines(&entries, truncated, "matches", TOOL_OUTPUT_LIMIT);

    ToolExecution {
        content: output_text,
        raw_output: serde_json::json!({
            "pattern": parsed_arguments.pattern,
            "matches": entries,
            "truncated": truncated,
        }),
        success: true,
        edit: None,
    }
}

pub(crate) fn grep_tool_execution(
    call: &ChatToolCall,
    context: &ToolContext,
    cancellation: &CancellationToken,
) -> ToolExecution {
    let parsed_arguments = match serde_json::from_str::<GrepArguments>(call.arguments()) {
        Ok(arguments) => arguments,
        Err(error) => return ToolExecution::failed(format!("invalid grep arguments: {error}")),
    };

    let matcher = match RegexMatcher::new_line_matcher(&parsed_arguments.pattern) {
        Ok(matcher) => matcher,
        Err(error) => return ToolExecution::failed(format!("invalid grep regex: {error}")),
    };

    let (mut grep_hits, truncated) =
        match collect_grep_matches(&context.cwd, &matcher, cancellation) {
            Ok(result) => result,
            Err(error) => return ToolExecution::failed(error),
        };

    grep_hits.sort_unstable_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.text.cmp(&right.text))
    });
    let lines = grep_hits
        .into_iter()
        .take(TOOL_OUTPUT_LIMIT)
        .map(|entry| format!("{}:{}:{}", entry.path, entry.line, entry.text))
        .collect::<Vec<_>>();
    let output_text = render_tool_lines(&lines, truncated, "matches", TOOL_OUTPUT_LIMIT);

    ToolExecution {
        content: truncate_tool_output(&output_text, FILE_OUTPUT_LIMIT).0,
        raw_output: serde_json::json!({
            "pattern": parsed_arguments.pattern,
            "matches": lines,
            "truncated": truncated,
        }),
        success: true,
        edit: None,
    }
}

pub(crate) async fn require_tool_permission(
    store: &SessionStore,
    context: &ToolContext,
    call: &ChatToolCall,
    kind: ToolKind,
    requester: Option<&dyn PermissionRequester>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let requester = requester.ok_or_else(|| {
        format!(
            "{} requires a client connection that can request permissions",
            call.name()
        )
    })?;

    match request_tool_permission(store, context, call, kind, requester, cancellation).await {
        Ok(
            PermissionDecision::AllowOnce
            | PermissionDecision::AllowAlways
            | PermissionDecision::AllowByMode,
        ) => Ok(()),
        Ok(PermissionDecision::RejectOnce | PermissionDecision::RejectAlways) => {
            Err(format!("{} was rejected by permission policy", call.name()))
        }
        Ok(PermissionDecision::Cancelled) => {
            Err(format!("{} permission request was cancelled", call.name()))
        }
        Err(error) => Err(format!(
            "failed to request permission for {}: {error}",
            call.name()
        )),
    }
}

async fn read_file_from_client<'a>(
    connection: &'a dyn ReadTextFileRequester,
    session_id: &'a str,
    path: &'a ConfinedPath,
    line: u32,
    limit: u32,
) -> Result<String, String> {
    let path = client_file_path(path).await?;
    let response = connection
        .read_text_file(
            ReadTextFileRequest::new(session_id.to_string(), path.clone())
                .line(line)
                .limit(limit),
        )
        .await
        .map_err(|error| read_file_client_error(&path, &error.to_string()))?;

    Ok(response.content)
}

async fn read_full_file_from_client<'a>(
    connection: &'a dyn ReadTextFileRequester,
    session_id: &'a str,
    path: &'a ConfinedPath,
) -> Result<String, String> {
    let path = client_file_path(path).await?;
    let response = connection
        .read_text_file(ReadTextFileRequest::new(
            session_id.to_string(),
            path.clone(),
        ))
        .await
        .map_err(|error| read_file_client_error(&path, &error.to_string()))?;

    Ok(response.content)
}

async fn read_edit_source(
    context: &ToolContext,
    path: &ConfinedPath,
    read_connection: Option<&dyn ReadTextFileRequester>,
    cancellation: &CancellationToken,
) -> Result<String, String> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err("edit_file cancelled".to_owned()),
        result = async {
            if context.client_capabilities.as_ref().is_some_and(|caps| caps.read_text_file) {
                let connection = read_connection.ok_or_else(||
                    "edit_file needs a client connection for fs/read_text_file".to_owned())?;
                read_full_file_from_client(connection, &context.session_id, path).await
            } else {
                let path = path.clone();
                blocking::unblock(move || {
                    path.read_to_string().map_err(|error| {
                        format!("failed to read {} before editing: {error}", path.path.display())
                    })
                }).await
            }
        } => result,
    }
}

async fn read_existing_text(
    context: &ToolContext,
    path: &ConfinedPath,
    read_connection: Option<&dyn ReadTextFileRequester>,
    use_client_write: bool,
) -> Result<Option<String>, String> {
    if use_client_write {
        let can_client_read = context
            .client_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.read_text_file);
        if !can_client_read {
            return Ok(None);
        }
        let Some(connection) = read_connection else {
            return Ok(None);
        };
        let path = client_file_path(path).await?;
        return match connection
            .read_text_file(ReadTextFileRequest::new(
                context.session_id.clone(),
                path.clone(),
            ))
            .await
        {
            Ok(response) => Ok(Some(response.content)),
            Err(error) if error.code == agent_client_protocol::ErrorCode::ResourceNotFound => {
                Ok(None)
            }
            Err(error) => Err(read_file_client_error(&path, &error.to_string())),
        };
    }

    let path = path.clone();
    blocking::unblock(move || match path.read_to_string() {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(read_file_local_error(&path.path, &error)),
    })
    .await
}

pub(crate) async fn write_file_to_client(
    connection: &dyn WriteTextFileRequester,
    store: &SessionStore,
    session_id: &str,
    path: &ConfinedPath,
    content: &str,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let path = client_file_path(path).await?;
    if cancellation.is_cancelled() {
        return Err("file write cancelled".to_owned());
    }
    require_tool_access(store, session_id, "write_file")?;
    connection
        .write_text_file(WriteTextFileRequest::new(
            session_id.to_string(),
            path.clone(),
            content.to_owned(),
        ))
        .await
        .map_err(|error| {
            format!(
                "failed to write {} through client fs/write_text_file: {error}",
                path.display()
            )
        })?;

    Ok(())
}

async fn write_file_to_local(
    store: &SessionStore,
    session_id: &str,
    path: &ConfinedPath,
    content: &str,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let path = path.clone();
    let content = content.to_owned();
    let cancellation = cancellation.clone();
    let store = store.clone();
    let session_id = session_id.to_owned();
    blocking::unblock(move || {
        if cancellation.is_cancelled() {
            return Err("file write cancelled".to_owned());
        }
        require_tool_access(&store, &session_id, "write_file")?;
        path.write(&content)
            .map_err(|error| format!("failed to write {}: {error}", path.path.display()))
    })
    .await
}

// Preflight reads and path verification may yield after the original approval.
// Apply this backstop both at dispatch and immediately before file mutation.
fn require_tool_access(
    store: &SessionStore,
    session_id: &str,
    tool_name: &str,
) -> Result<(), String> {
    if store
        .selected_content_limits(session_id)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("selected-content Sessions refuse all tool calls".to_owned());
    }
    let mode = store
        .session_behavior(session_id)
        .map_err(|error| error.to_string())?;
    if mode.allows_tool_kind(AdapterToolRegistry.kind(tool_name)) {
        Ok(())
    } else {
        Err(format!(
            "{} mode refuses {tool_name} tool calls",
            mode.mode_id()
        ))
    }
}

fn line_number_for_offset(text: &str, offset: usize) -> u32 {
    let Some(prefix) = text.get(..offset) else {
        return 1;
    };
    let line = prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1);

    u32::try_from(line).unwrap_or(u32::MAX)
}

pub(crate) async fn read_file_from_local(
    path: &ConfinedPath,
    line: u32,
    limit: u32,
) -> Result<String, String> {
    let path = path.clone();
    let text = blocking::unblock(move || {
        path.read_to_string()
            .map_err(|error| read_file_local_error(&path.path, &error))
    })
    .await?;
    let lines: Vec<&str> = text.lines().collect();

    let start_index = usize::try_from(line.saturating_sub(1))
        .map_err(|error| format!("line number is too large: {error}"))?;
    let max_lines =
        usize::try_from(limit).map_err(|error| format!("line limit is too large: {error}"))?;

    let content = lines
        .iter()
        .skip(start_index)
        .take(max_lines)
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");

    Ok(content)
}

async fn local_file_is_non_utf8(path: &ConfinedPath) -> bool {
    let path = path.clone();
    blocking::unblock(move || {
        path.read_to_string()
            .is_err_and(|error| error.kind() == ErrorKind::InvalidData)
    })
    .await
}

pub(crate) fn read_file_local_error(path: &Path, error: &std::io::Error) -> String {
    if error.kind() == ErrorKind::InvalidData {
        return non_utf8_file_message(path);
    }

    format!("failed to read {}: {error}", path.display())
}

pub(crate) fn read_file_client_error(path: &Path, message: &str) -> String {
    if is_utf8_error_message(message) {
        return non_utf8_file_message(path);
    }

    format!(
        "failed to read {} through client fs/read_text_file: {message}",
        path.display()
    )
}

pub(crate) fn non_utf8_file_message(path: &Path) -> String {
    format!(
        "read_file only supports UTF-8 text files; {} appears to be binary or non-UTF-8",
        path.display()
    )
}

pub(crate) fn is_utf8_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("valid utf-8")
        || lower.contains("invalid utf-8")
        || lower.contains("non-utf-8")
        || lower.contains("utf8")
}

async fn resolve_tool_path(context: &ToolContext, path: &Path) -> Result<ConfinedPath, String> {
    let context = context.clone();
    let path = path.to_path_buf();
    blocking::unblock(move || {
        ConfinedPath::resolve(&context.cwd, &context.additional_directories, &path)
            .map_err(|error| error.to_string())
    })
    .await
}

async fn client_file_path(path: &ConfinedPath) -> Result<PathBuf, String> {
    let path = path.clone();
    blocking::unblock(move || path.path_for_client().map_err(|error| error.to_string())).await
}

pub(crate) fn collect_directory_entries(path: &ConfinedPath) -> Result<Vec<String>, String> {
    let mut entries = path
        .directory()
        .and_then(|directory| directory.entries())
        .map_err(|error| format!("failed to read directory {}: {error}", path.path.display()))?
        .map(|entry| {
            entry.map_err(|error| {
                format!(
                    "failed to read directory entry in {}: {error}",
                    path.path.display()
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    entries.sort_unstable_by(|left, right| {
        left.file_name()
            .to_string_lossy()
            .cmp(&right.file_name().to_string_lossy())
    });

    Ok(entries
        .into_iter()
        .map(|entry| {
            let display = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => format!("{display}/"),
                _ => display,
            }
        })
        .collect())
}

pub(crate) fn render_tool_lines(
    lines: &[String],
    truncated: bool,
    label: &str,
    limit: usize,
) -> String {
    let mut output = lines.join("\n");

    if truncated {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = write!(output, "... truncated after {limit} {label}");
    }

    output
}

pub(crate) fn render_command_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> String {
    let mut output = String::new();

    if !stdout.is_empty() {
        output.push_str("stdout:\n");
        output.push_str(stdout);
        if !stdout.ends_with('\n') {
            output.push('\n');
        }
    }

    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("stderr:\n");
        output.push_str(stderr);
        if !stderr.ends_with('\n') {
            output.push('\n');
        }
    }

    if output.is_empty() {
        let status = exit_code.map_or_else(|| "signal".to_string(), |code| code.to_string());
        let _ = write!(output, "command exited with status {status}");
    }

    output
}

pub(crate) fn truncate_tool_output(output: &str, limit: usize) -> (String, bool) {
    let truncated = output.chars().count() > limit;
    if !truncated {
        return (output.to_string(), false);
    }

    let mut content = output.chars().take(limit).collect::<String>();
    let _ = write!(content, "\n... truncated after {limit} characters");
    (content, true)
}

#[derive(Debug, Clone)]
struct GrepMatch {
    path: String,
    line: u64,
    text: String,
}

fn collect_grep_matches(
    root: &Path,
    matcher: &RegexMatcher,
    cancellation: &CancellationToken,
) -> Result<(Vec<GrepMatch>, bool), String> {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let mut grep_hits = Vec::<GrepMatch>::new();
    let mut truncated = false;

    let root =
        ConfinedPath::resolve(root, &[], Path::new(".")).map_err(|error| error.to_string())?;
    walk_files(&root, cancellation, |relative_path, entry| {
        let file = entry.open().map_err(|error| error.to_string())?;
        let search_result = searcher.search_reader(
            matcher,
            SearchReader { file, cancellation },
            UTF8(|line_number, line| {
                if grep_hits.len() >= TOOL_OUTPUT_LIMIT {
                    truncated = true;
                    return Ok(false);
                }

                grep_hits.push(GrepMatch {
                    path: relative_path.display().to_string(),
                    line: line_number,
                    text: line.to_string(),
                });
                if grep_hits.len() >= TOOL_OUTPUT_LIMIT {
                    truncated = true;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }),
        );
        if let Err(error) = search_result {
            return Err(format!(
                "failed to grep {}: {error}",
                relative_path.display()
            ));
        }

        Ok(!truncated)
    })?;

    Ok((grep_hits, truncated))
}

impl ToolRegistry for AdapterToolRegistry {
    fn definitions(
        &self,
        context: &ToolContext,
        store: &crate::SessionStore,
    ) -> Result<Vec<ToolDefinition>, AdapterError> {
        if store
            .selected_content_limits(&context.session_id)?
            .is_some()
        {
            return Ok(Vec::new());
        }
        let mut definitions = vec![
            read_file_tool_definition(),
            list_dir_tool_definition(),
            glob_tool_definition(),
            grep_tool_definition(),
            write_file_tool_definition(),
            edit_file_tool_definition(),
            run_command_tool_definition(),
            update_plan_tool_definition(),
        ];
        if store.session_behavior(&context.session_id)? == crate::SessionBehavior::Plan {
            definitions.push(exit_plan_mode_tool_definition());
        }
        definitions.extend(store.mcp_definitions(&context.session_id)?);
        Ok(definitions)
    }

    fn kind(&self, name: &str) -> ToolKind {
        match name {
            "read_file" | "list_dir" => ToolKind::Read,
            "glob" | "grep" => ToolKind::Search,
            "write_file" | "edit_file" => ToolKind::Edit,
            "run_command" => ToolKind::Execute,
            "update_plan" | "exit_plan_mode" => ToolKind::Think,
            name if crate::is_mcp_tool_name(name) => crate::mcp_tool_kind(),
            _ => ToolKind::Other,
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ChatToolCall,
        context: &'a ToolContext,
        store: &'a crate::SessionStore,
        executor: Option<&'a dyn super::registry::ToolExecutor>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'a, ToolExecution> {
        match executor {
            Some(executor) => executor.execute(call, context, store, cancellation_token),
            None => execute_tools(call, context, store, None, cancellation_token),
        }
    }
}

/// Execute built-in/MCP effects at the tool I/O boundary.
pub(crate) fn execute_tools<'a>(
    call: &'a ChatToolCall,
    context: &'a ToolContext,
    store: &'a crate::SessionStore,
    connection: Option<&'a dyn crate::ToolCallRequester>,
    cancellation_token: CancellationToken,
) -> BoxFuture<'a, ToolExecution> {
    Box::pin(async move {
        if cancellation_token.is_cancelled() {
            return ToolExecution::failed(format!("{} cancelled", call.name()));
        }
        if let Err(error) = require_tool_access(store, &context.session_id, call.name()) {
            return ToolExecution::failed(error);
        }
        match call.name() {
            "read_file" => {
                let result = tokio::select! {
                    biased;
                    () = cancellation_token.cancelled() => return ToolExecution::failed("read_file cancelled"),
                    result = read_file_tool_execution(
                        call, context,
                        connection.map(|requester| requester as &dyn crate::ReadTextFileRequester),
                    ) => result,
                };
                // Cancellation may become ready during the response's final poll.
                if cancellation_token.is_cancelled() {
                    ToolExecution::failed("read_file cancelled")
                } else {
                    result
                }
            }
            "list_dir" => {
                let call = call.clone();
                let context = context.clone();
                blocking::unblock(move || list_dir_tool_execution(&call, &context)).await
            }
            "glob" | "grep" => {
                let call = call.clone();
                let context = context.clone();
                let cancellation = cancellation_token.child_token();
                // Dropping the ACP turn also stops its blocking traversal.
                let _cancel_on_drop = cancellation.clone().drop_guard();
                blocking::unblock(move || {
                    if call.name() == "glob" {
                        glob_tool_execution(&call, &context, &cancellation)
                    } else {
                        grep_tool_execution(&call, &context, &cancellation)
                    }
                })
                .await
            }
            "write_file" => {
                write_file_tool_execution(
                    store,
                    call,
                    context,
                    connection.map(|requester| requester as &dyn crate::ReadTextFileRequester),
                    connection.map(|requester| requester as &dyn crate::WriteTextFileRequester),
                    connection.map(|requester| requester as &dyn crate::PermissionRequester),
                    &cancellation_token,
                )
                .await
            }
            "edit_file" => {
                edit_file_tool_execution(
                    store,
                    call,
                    context,
                    connection.map(|requester| requester as &dyn crate::ReadTextFileRequester),
                    connection.map(|requester| requester as &dyn crate::WriteTextFileRequester),
                    connection.map(|requester| requester as &dyn crate::PermissionRequester),
                    &cancellation_token,
                )
                .await
            }
            "run_command" => {
                run_command_tool_execution(
                    store,
                    call,
                    context,
                    connection.map(|requester| requester as &dyn crate::PermissionRequester),
                    connection.map(|requester| requester as &dyn crate::TerminalRequester),
                    connection.map(|requester| requester as &dyn crate::ToolProgressReporter),
                    &cancellation_token,
                )
                .await
            }
            "update_plan" => update_plan_tool_execution(call),
            "exit_plan_mode" => {
                let store = store.clone();
                let call = call.clone();
                let context = context.clone();
                blocking::unblock(move || exit_plan_mode_tool_execution(&store, &call, &context))
                    .await
            }
            name if crate::is_mcp_tool_name(name) => {
                crate::mcp_tool_execution(
                    store,
                    call,
                    context,
                    connection.map(|requester| requester as &dyn crate::PermissionRequester),
                    &cancellation_token,
                )
                .await
            }
            _ => ToolExecution::failed(format!("unknown tool: {}", call.name())),
        }
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod confinement_tests;

#[cfg(test)]
mod edit_approval_tests;
