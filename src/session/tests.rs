#![allow(clippy::indexing_slicing)]
use super::{PendingToolCalls, PermissionDecision, ReasoningEffort, SessionBehavior};
use agent_client_protocol::schema::v1::SessionModeId;

/// Return type for [`permission_mode_fixture`].
pub(crate) type PermissionModeFixture = (
    crate::session::SessionStore,
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
        session_id: session.session_id.clone(),
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

#[test]
fn reasoning_effort_name_and_description() {
    assert_eq!(ReasoningEffort::High.name(), "High");
    assert_eq!(ReasoningEffort::Max.name(), "Max");
    assert!(
        ReasoningEffort::High
            .description()
            .contains("Default DeepSeek")
    );
    assert!(
        ReasoningEffort::Max
            .description()
            .contains("Maximum DeepSeek")
    );
}

#[test]
fn reasoning_effort_from_value_id_rejects_unknown() {
    assert!(
        ReasoningEffort::from_value_id(
            &agent_client_protocol::schema::v1::SessionConfigValueId::new("bogus")
        )
        .is_none()
    );
}

#[test]
fn max_tokens_value_id_round_trips_default_and_preset() {
    use agent_client_protocol::schema::v1::SessionConfigValueId;

    assert_eq!(super::max_tokens_value_id(None), "default");
    assert_eq!(super::max_tokens_value_id(Some(8_192)), "8192");

    assert_eq!(
        super::max_tokens_from_value_id(&SessionConfigValueId::new("default")).ok(),
        Some(None)
    );
    assert_eq!(
        super::max_tokens_from_value_id(&SessionConfigValueId::new("8192")).ok(),
        Some(Some(8_192))
    );
}

#[test]
fn max_tokens_from_value_id_rejects_zero_and_non_numeric() {
    use agent_client_protocol::schema::v1::SessionConfigValueId;

    assert!(super::max_tokens_from_value_id(&SessionConfigValueId::new("0")).is_err());
    assert!(super::max_tokens_from_value_id(&SessionConfigValueId::new("bogus")).is_err());
}

#[test]
fn max_tokens_select_options_include_default_and_presets() {
    let options = super::max_tokens_select_options();
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

#[test]
fn pending_tool_calls_require_complete_metadata() -> Result<(), agent_client_protocol::Error> {
    use acp_llm_adapter::llm::ToolCallDelta;

    let mut missing_id = PendingToolCalls::default();
    missing_id.push(&ToolCallDelta::new(
        1,
        None,
        Some("echo".to_string()),
        Some("{}".to_string()),
    ));
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
    ));
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
    use agent_client_protocol::schema::v1::ToolKind;

    assert_eq!(SessionBehavior::Ask.mode_id().0.as_ref(), "ask");
    assert_eq!(SessionBehavior::Ask.name(), "Ask");
    assert_eq!(
        SessionBehavior::AcceptEdits.mode_id().0.as_ref(),
        "accept-edits"
    );
    assert_eq!(SessionBehavior::AcceptEdits.name(), "Accept edits");
    assert_eq!(SessionBehavior::Plan.mode_id().0.as_ref(), "plan");
    assert_eq!(SessionBehavior::Plan.name(), "Plan");
    assert_eq!(SessionBehavior::Yolo.mode_id().0.as_ref(), "yolo");
    assert_eq!(SessionBehavior::Yolo.name(), "Yolo");
    assert_eq!(
        SessionBehavior::from_mode_id(&SessionModeId::new("ask")),
        Some(SessionBehavior::Ask)
    );
    assert_eq!(
        SessionBehavior::from_mode_id_str("accept-edits"),
        Some(SessionBehavior::AcceptEdits)
    );
    assert_eq!(
        SessionBehavior::from_mode_id(&SessionModeId::new("accept-edits")),
        Some(SessionBehavior::AcceptEdits)
    );
    assert_eq!(
        SessionBehavior::from_mode_id(&SessionModeId::new("plan")),
        Some(SessionBehavior::Plan)
    );
    assert_eq!(
        SessionBehavior::from_mode_id(&SessionModeId::new("yolo")),
        Some(SessionBehavior::Yolo)
    );
    assert_eq!(
        SessionBehavior::from_mode_id(&SessionModeId::new("bogus")),
        None
    );
    assert!(!SessionBehavior::Ask.allows_without_prompt(ToolKind::Edit));
    assert!(SessionBehavior::AcceptEdits.allows_without_prompt(ToolKind::Edit));
    assert!(!SessionBehavior::AcceptEdits.allows_without_prompt(ToolKind::Execute));
    assert!(!SessionBehavior::Plan.allows_without_prompt(ToolKind::Execute));
    assert!(SessionBehavior::Yolo.allows_without_prompt(ToolKind::Execute));
    assert!(!SessionBehavior::Yolo.allows_without_prompt(ToolKind::Read));
    assert!(is_mcp_tool_name("mcp__server__tool"));
    assert_eq!(mcp_tool_kind(), ToolKind::Execute);
}
