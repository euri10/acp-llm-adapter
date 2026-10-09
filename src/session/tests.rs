#![allow(clippy::indexing_slicing)]
use super::{PendingToolCalls, PermissionDecision, ReasoningEffort, SessionBehavior};
use crate::tools::ToolKind;
use acp_llm_adapter::error::AdapterError;
use acp_llm_adapter::llm::{ChatError, ToolCall, ToolCallDelta};
use agent_client_protocol::schema::v1::{
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use tokio_util::sync::CancellationToken;

#[test_log::test(tokio::test)]
async fn clear_history_preserves_settings_permissions_and_spend()
-> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("acp-clear-store-{}", uuid::Uuid::new_v4()));
    let store = crate::test_store()
        .with_persistence(crate::session_store::FilesystemSessionStore::new(&root));
    let session = crate::acp::handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?
    .session_id;
    let history = vec![acp_llm_adapter::llm::ChatMessage::user(
        "private old context",
    )];
    store.save_history(&session.0, &history)?;
    store.set_model(&session.0, "deepseek-v4-pro".into())?;
    store.set_reasoning_effort(&session.0, ReasoningEffort::Max)?;
    store.set_max_tokens(&session.0, Some(4096))?;
    store.set_mode(&session.0, SessionBehavior::Plan)?;
    store.add_cost_micros(&session.0, 1234)?;
    store.add_always_allow(&session.0, "write_file".into())?;
    store.add_always_reject(&session.0, "run_command".into())?;
    store.clear_history(&session.0).await?;
    store.with_session(&session.0, |record| {
        assert!(record.history.is_empty());
        assert_eq!(record.cost_micros, 1234);
        assert!(record.permission_allow_always.contains("write_file"));
        assert!(record.permission_reject_always.contains("run_command"));
        assert!(record.active_turn.is_none());
        Ok(())
    })?;
    let persisted = store.load_persisted_record(&session.0)?;
    assert!(persisted.history.is_empty());
    assert_eq!(persisted.meta.cost_micros, 1234);
    assert_eq!(persisted.meta.mode, SessionBehavior::Plan);
    assert_eq!(persisted.meta.model, "deepseek-v4-pro");
    assert_eq!(persisted.meta.reasoning_effort, ReasoningEffort::Max);
    assert_eq!(persisted.meta.max_tokens, Some(4096));
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn clear_history_failure_keeps_memory_and_disk_conversation()
-> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("acp-clear-failure-{}", uuid::Uuid::new_v4()));
    let store = crate::test_store()
        .with_persistence(crate::session_store::FilesystemSessionStore::new(&root));
    let session = crate::acp::handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?
    .session_id;
    let history = vec![acp_llm_adapter::llm::ChatMessage::user("keep this context")];
    store.save_history(&session.0, &history)?;
    let staging_path = root
        .join("sessions")
        .join(session.0.as_ref())
        .join("history.jsonl.tmp");
    std::fs::create_dir(&staging_path)?;
    assert!(store.clear_history(&session.0).await.is_err());
    store.with_session(&session.0, |record| {
        assert_eq!(record.history, history);
        assert!(record.active_turn.is_none());
        Ok(())
    })?;
    assert_eq!(store.load_persisted_record(&session.0)?.history, history);
    std::fs::remove_dir(staging_path)?;
    store.clear_history(&session.0).await?;
    assert!(store.load_persisted_record(&session.0)?.history.is_empty());
    std::fs::remove_dir_all(root)?;
    Ok(())
}

/// Return type for [`permission_mode_fixture`].
pub(crate) type PermissionModeFixture = (
    crate::SessionStore,
    agent_client_protocol::schema::v1::SessionId,
    crate::tools::ToolContext,
    acp_llm_adapter::llm::ToolCall,
    acp_llm_adapter::llm::ToolCall,
);

/// Create a fully wired permission-mode test environment.
///
/// Returns `(store, session_id, context, edit_call, shell_call)`.
///
/// # Errors
///
/// Propagates errors from session creation.
pub(crate) fn permission_mode_fixture()
-> Result<PermissionModeFixture, agent_client_protocol::Error> {
    use crate::test_store;
    let store = test_store();
    let session = crate::acp::handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?;
    let context = crate::tools::ToolContext {
        session_id: session.session_id.0.to_string(),
        cwd: std::path::PathBuf::from("/tmp"),
        additional_directories: Vec::new(),
        client_capabilities: None,
    };
    let edit_call = acp_llm_adapter::llm::ToolCall::new(
        "call-edit",
        "write_file",
        serde_json::json!({ "path": "file.txt" }).to_string(),
    );
    let shell_call = acp_llm_adapter::llm::ToolCall::new(
        "call-shell",
        "run_command",
        serde_json::json!({ "command": "echo hi" }).to_string(),
    );

    Ok((
        store.clone(),
        session.session_id,
        context,
        edit_call,
        shell_call,
    ))
}

#[test]
fn permission_decision_debug_impl_is_callable() {
    let decisions = [
        PermissionDecision::AllowOnce,
        PermissionDecision::AllowAlways,
        PermissionDecision::AllowByMode,
        PermissionDecision::RejectOnce,
        PermissionDecision::RejectAlways,
        PermissionDecision::Cancelled,
    ];
    for decision in &decisions {
        let _ = format!("{decision:?}");
    }
}

#[test_log::test(tokio::test)]
async fn reject_always_is_remembered_before_mode_auto_approval()
-> Result<(), agent_client_protocol::Error> {
    let (store, session_id, context, _, call) = permission_mode_fixture()?;
    let requester =
        crate::test_utils::FakePermissionRequester::new(vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                super::PERMISSION_REJECT_ALWAYS_OPTION_ID,
            )),
        )]);
    assert_eq!(
        crate::request_tool_permission(
            &store,
            &context,
            &call,
            ToolKind::Execute,
            &requester,
            None,
            &CancellationToken::new()
        )
        .await?,
        PermissionDecision::RejectAlways
    );
    store.set_mode(&session_id.0, SessionBehavior::Yolo)?;
    let requester = crate::test_utils::FakePermissionRequester::new(Vec::new());
    assert_eq!(
        crate::request_tool_permission(
            &store,
            &context,
            &call,
            ToolKind::Execute,
            &requester,
            None,
            &CancellationToken::new()
        )
        .await?,
        PermissionDecision::RejectAlways,
        "YOLO must not discard a remembered editor denial"
    );
    Ok(())
}

#[test]
fn reasoning_effort_name_and_description() {
    assert_eq!(ReasoningEffort::High.name(), "High");
    assert_eq!(ReasoningEffort::Max.name(), "Max");
    assert!(
        ReasoningEffort::Default
            .description()
            .contains("default reasoning")
    );
    assert!(
        ReasoningEffort::Max
            .description()
            .contains("Maximum reasoning")
    );
}

#[test]
fn reasoning_effort_from_value_id_rejects_unknown() {
    assert!(ReasoningEffort::from_value_id("bogus").is_none());
}

#[test]
fn max_tokens_value_id_round_trips_default_and_preset() {
    assert_eq!(
        crate::acp::session_options::max_tokens_value_id(None),
        "default"
    );
    assert_eq!(
        crate::acp::session_options::max_tokens_value_id(Some(8_192)),
        "8192"
    );

    assert_eq!(super::max_tokens_from_value_id("default").ok(), Some(None));
    assert_eq!(
        super::max_tokens_from_value_id("8192").ok(),
        Some(Some(8_192))
    );
}

#[test]
fn max_tokens_from_value_id_rejects_zero_and_non_numeric() {
    assert!(super::max_tokens_from_value_id("0").is_err());
    assert!(super::max_tokens_from_value_id("bogus").is_err());
}

#[test]
fn max_tokens_select_options_include_default_and_presets() {
    let options = crate::acp::session_options::max_tokens_select_options();
    assert!(
        options
            .iter()
            .any(|option| option.value.0.as_ref() == "default")
    );
    assert!(
        options
            .iter()
            .any(|option| option.value.0.as_ref() == "4096")
    );
    assert!(
        options
            .iter()
            .any(|option| option.value.0.as_ref() == "131072")
    );
}

#[test_log::test]
fn pending_tool_calls_reject_out_of_range_index_without_growth() -> Result<(), AdapterError> {
    for populated in [false, true] {
        let mut pending = PendingToolCalls::default();
        if populated {
            pending.push(&ToolCallDelta::new(
                0,
                Some("kept".into()),
                Some("echo".into()),
                Some("{}".into()),
            ))?;
        }
        let len = pending.calls.len();
        let capacity = pending.calls.capacity();
        // Test the cheap boundary first so removing the guard fails before the
        // extreme index can cause an unbounded allocation in this regression.
        for index in [super::MAX_TOOL_CALLS_PER_COMPLETION, usize::MAX] {
            assert!(matches!(
                pending.push(&ToolCallDelta::new(index, None, None, None)),
                Err(AdapterError::Llm(ChatError::InvalidResponse(_)))
            ));
            assert_eq!(
                pending.calls.len(),
                len,
                "invalid index allocated call slots"
            );
            assert_eq!(pending.calls.capacity(), capacity);
        }
        assert_eq!(
            pending.finish()?,
            if populated {
                vec![ToolCall::new("kept", "echo", "{}")]
            } else {
                vec![]
            }
        );
    }
    Ok(())
}

#[test_log::test]
fn pending_tool_calls_accept_interleaved_fragments_up_to_the_limit() -> Result<(), AdapterError> {
    let mut pending = PendingToolCalls::default();
    for index in (0..super::MAX_TOOL_CALLS_PER_COMPLETION).rev() {
        pending.push(&ToolCallDelta::new(
            index,
            Some(format!("call-{index}")),
            Some("echo".into()),
            Some(format!("{{\"index\":{index}")),
        ))?;
    }
    for index in 0..super::MAX_TOOL_CALLS_PER_COMPLETION {
        pending.push(&ToolCallDelta::new(index, None, None, Some("}".into())))?;
    }
    let calls = pending.finish()?;
    assert_eq!(calls.len(), super::MAX_TOOL_CALLS_PER_COMPLETION);
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(
            call,
            &ToolCall::new(
                format!("call-{index}"),
                "echo",
                format!("{{\"index\":{index}}}")
            )
        );
    }
    Ok(())
}

#[test]
fn pending_tool_calls_require_complete_metadata() -> Result<(), agent_client_protocol::Error> {
    use acp_llm_adapter::llm::ToolCallDelta;

    let mut missing_id = PendingToolCalls::default();
    missing_id.push(&ToolCallDelta::new(
        1,
        None,
        Some("echo".to_string()),
        Some("{}".to_string()),
    ))?;
    let Err(error) = missing_id.finish() else {
        return Err(agent_client_protocol::Error::internal_error()
            .data("expected missing tool call id to fail"));
    };
    assert!(error.to_string().contains("missing an id"));

    let mut missing_name = PendingToolCalls::default();
    missing_name.push(&ToolCallDelta::new(
        0,
        Some("call-1".to_string()),
        None,
        Some("{}".to_string()),
    ))?;
    let Err(error) = missing_name.finish() else {
        return Err(agent_client_protocol::Error::internal_error()
            .data("expected missing tool call name to fail"));
    };
    assert!(error.to_string().contains("missing a function name"));

    Ok(())
}

#[test]
fn session_behavior_helpers_cover_all_branches() {
    use crate::mcp::{is_mcp_tool_name, mcp_tool_kind};

    assert_eq!(SessionBehavior::Ask.mode_id(), "ask");
    assert_eq!(SessionBehavior::Ask.name(), "Ask");
    assert_eq!(SessionBehavior::AcceptEdits.mode_id(), "accept-edits");
    assert_eq!(SessionBehavior::AcceptEdits.name(), "Accept edits");
    assert_eq!(SessionBehavior::Plan.mode_id(), "plan");
    assert_eq!(SessionBehavior::Plan.name(), "Plan");
    assert_eq!(SessionBehavior::Yolo.mode_id(), "yolo");
    assert_eq!(SessionBehavior::Yolo.name(), "Yolo");
    assert_eq!(
        SessionBehavior::from_mode_id_str("ask"),
        Some(SessionBehavior::Ask)
    );
    assert_eq!(
        SessionBehavior::from_mode_id_str("accept-edits"),
        Some(SessionBehavior::AcceptEdits)
    );
    assert_eq!(
        SessionBehavior::from_mode_id_str("accept-edits"),
        Some(SessionBehavior::AcceptEdits)
    );
    assert_eq!(
        SessionBehavior::from_mode_id_str("plan"),
        Some(SessionBehavior::Plan)
    );
    assert_eq!(
        SessionBehavior::from_mode_id_str("yolo"),
        Some(SessionBehavior::Yolo)
    );
    assert_eq!(SessionBehavior::from_mode_id_str("bogus"), None);
    assert!(!SessionBehavior::Ask.allows_without_prompt(ToolKind::Edit));
    assert!(SessionBehavior::AcceptEdits.allows_without_prompt(ToolKind::Edit));
    assert!(!SessionBehavior::AcceptEdits.allows_without_prompt(ToolKind::Execute));
    assert!(!SessionBehavior::Plan.allows_without_prompt(ToolKind::Execute));
    assert!(SessionBehavior::Yolo.allows_without_prompt(ToolKind::Execute));
    assert!(!SessionBehavior::Yolo.allows_without_prompt(ToolKind::Read));
    assert!(is_mcp_tool_name("mcp__server__tool"));
    assert_eq!(mcp_tool_kind(), ToolKind::Execute);
}
