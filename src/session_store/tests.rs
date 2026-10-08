#![allow(clippy::indexing_slicing)]
use super::{FilesystemSessionStore, PersistedSessionMeta};
use crate::{ReasoningEffort, SessionBehavior};
use acp_llm_adapter::llm::ChatMessage;
use uuid::Uuid;

fn idle_record(cwd: &str) -> Result<crate::session::SessionRecord, Box<dyn std::error::Error>> {
    let store = crate::test_store();
    let session = crate::acp::handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new(cwd),
    )?
    .session_id;
    store
        .state
        .lock()
        .map_err(|error| error.to_string())?
        .sessions
        .remove(session.0.as_ref())
        .ok_or_else(|| "fixture session missing".into())
}

#[test_log::test]
fn session_publication_preserves_active_turn_and_resources()
-> Result<(), Box<dyn std::error::Error>> {
    let store = crate::test_store();
    let session = crate::acp::handle_new_session_request(
        &store,
        &agent_client_protocol::schema::v1::NewSessionRequest::new("/tmp"),
    )?
    .session_id;
    let replacement_record = idle_record("/replacement")?;
    let token = tokio_util::sync::CancellationToken::new();
    store.begin_turn(&session.0, token.clone(), ChatMessage::user("active"), None)?;
    let result = store.insert_session_with_resources(
        session.0.to_string(),
        replacement_record,
        vec![serde_json::json!({"name": "replacement"})],
        Vec::new(),
    );
    assert!(
        matches!(
            result,
            Err(acp_llm_adapter::error::AdapterError::InvalidRequest(_))
        ),
        "publication replaced an active turn: {result:?}"
    );
    store.cancel_active_turn(&session.0)?;
    assert!(token.is_cancelled(), "original cancellation owner was lost");
    assert!(
        store
            .insert_session_with_resources(
                session.0.to_string(),
                idle_record("/replacement")?,
                Vec::new(),
                Vec::new(),
            )
            .is_err(),
        "a cancelled turn still owns the session until cleanup finishes"
    );
    store.with_session(&session.0, |record| {
        assert_eq!(record.cwd, std::path::Path::new("/tmp"));
        assert!(record.active_turn.is_some());
        Ok(())
    })?;
    assert!(
        store
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .resources
            .get(session.0.as_ref())
            .is_some_and(|resources| resources.servers.is_empty())
    );
    assert!(
        store
            .begin_turn(
                &session.0,
                tokio_util::sync::CancellationToken::new(),
                ChatMessage::user("overlapping"),
                None,
            )
            .is_err(),
        "cancellation must retain ownership until cleanup completes"
    );
    store.clear_active_turn(&session.0, &token)?;
    store.insert_session_with_resources(
        session.0.to_string(),
        idle_record("/replacement")?,
        Vec::new(),
        Vec::new(),
    )?;
    store.with_session(&session.0, |record| {
        assert_eq!(record.cwd, std::path::Path::new("/replacement"));
        Ok(())
    })?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn session_publication_rechecks_turn_admitted_during_async_setup()
-> Result<(), Box<dyn std::error::Error>> {
    let store = crate::test_store();
    let id = "restore-race";
    store.insert_session_with_resources(id.into(), idle_record("/tmp")?, Vec::new(), Vec::new())?;
    let (setup_done, setup_wait) = tokio::sync::oneshot::channel();
    let (publish, publish_wait) = tokio::sync::oneshot::channel();
    let replacement = idle_record("/replacement")?;
    let restore = {
        let store = store.clone();
        tokio::spawn(async move {
            let _ = setup_done.send(());
            publish_wait.await.map_err(|error| error.to_string())?;
            Ok::<_, String>(store.insert_session_with_resources(
                id.into(),
                replacement,
                Vec::new(),
                Vec::new(),
            ))
        })
    };
    setup_wait.await?;
    let first = tokio_util::sync::CancellationToken::new();
    store.begin_turn(id, first.clone(), ChatMessage::user("first"), None)?;
    publish.send(()).map_err(|()| "restore dropped gate")?;
    assert!(restore.await??.is_err());
    store.cancel_active_turn(id)?;
    assert!(first.is_cancelled());
    store.clear_active_turn(id, &first)?;
    let second = tokio_util::sync::CancellationToken::new();
    store.begin_turn(id, second.clone(), ChatMessage::user("second"), None)?;
    assert!(!second.is_cancelled());
    store.cancel_active_turn(id)?;
    assert!(
        second.is_cancelled(),
        "next prompt lost its cancellation owner"
    );
    Ok(())
}

#[test_log::test]
fn closing_an_active_session_cancels_its_owned_turn() -> Result<(), Box<dyn std::error::Error>> {
    let store = crate::test_store();
    let id = "close-active";
    store.insert_session_with_resources(id.into(), idle_record("/tmp")?, Vec::new(), Vec::new())?;
    let token = tokio_util::sync::CancellationToken::new();
    store.begin_turn(id, token.clone(), ChatMessage::user("active"), None)?;
    assert!(store.remove_session(id)?);
    assert!(token.is_cancelled(), "session/close abandoned ongoing work");
    Ok(())
}

#[test_log::test]
fn removed_active_session_cannot_be_republished_before_cleanup()
-> Result<(), Box<dyn std::error::Error>> {
    for delete in [false, true] {
        let root = std::env::temp_dir().join(format!("acp-retired-turn-{}", Uuid::new_v4()));
        let store = crate::test_store().with_persistence(FilesystemSessionStore::new(&root));
        let id = "removed-active";
        store.insert_session_with_resources(
            id.into(),
            idle_record("/tmp")?,
            Vec::new(),
            Vec::new(),
        )?;
        let original_history = vec![ChatMessage::user("before removal")];
        store.save_history(id, &original_history)?;
        let token = tokio_util::sync::CancellationToken::new();
        store.begin_turn(id, token.clone(), ChatMessage::user("active"), None)?;
        if delete {
            store.delete_session(id)?;
        } else {
            store.remove_session(id)?;
        }
        assert!(
            store
                .insert_session_with_resources(
                    id.into(),
                    idle_record("/tmp")?,
                    Vec::new(),
                    Vec::new()
                )
                .is_err(),
            "removed active owner was replaced before its cleanup (delete={delete})"
        );
        // A tool's cancellation result may complete after removal; it cannot
        // recreate deleted history or append to the snapshot retained by close.
        store.save_history(id, &[ChatMessage::user("stale completion")])?;
        if delete {
            assert!(!root.join("sessions").join(id).exists());
        } else {
            assert_eq!(store.load_persisted_record(id)?.history, original_history);
        }
        store.clear_active_turn(id, &tokio_util::sync::CancellationToken::new())?;
        assert!(
            store
                .state
                .lock()
                .map_err(|error| error.to_string())?
                .retired_turns
                .contains_key(id)
        );
        store.clear_active_turn(id, &token)?;
        store.insert_session_with_resources(
            id.into(),
            idle_record("/tmp")?,
            Vec::new(),
            Vec::new(),
        )?;
        let next = tokio_util::sync::CancellationToken::new();
        store.begin_turn(id, next.clone(), ChatMessage::user("next"), None)?;
        store.clear_active_turn(id, &token)?;
        store.cancel_active_turn(id)?;
        assert!(next.is_cancelled(), "stale cleanup cleared a newer owner");
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test_log::test]
fn round_trips_session_metadata_and_history()
-> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let state_dir = std::env::temp_dir().join(format!("acp-llm-session-store-{}", Uuid::new_v4()));
    let cwd = state_dir.join("workspace");
    let store = FilesystemSessionStore::new(&state_dir);
    let meta = PersistedSessionMeta {
        session_id: "session-roundtrip".to_string(),
        cwd: cwd.clone(),
        additional_directories: vec![state_dir.join("extra")],
        mode: SessionBehavior::Plan,
        model: "deepseek-v4-pro".to_string(),
        reasoning_effort: ReasoningEffort::Max,
        max_tokens: Some(8_192),
        mcp_servers: Vec::new(),
        title: None,
        updated_at: None,
        cost_micros: 0,
    };

    store.persist_turn(&meta, &[ChatMessage::user("hello")])?;
    store.persist_turn(&meta, &[ChatMessage::assistant("world")])?;

    let record = store.load_record("session-roundtrip")?;
    assert_eq!(record.meta, meta);
    assert_eq!(record.history.len(), 2);
    assert_eq!(record.history[0], ChatMessage::user("hello"));
    assert_eq!(record.history[1], ChatMessage::assistant("world"));

    let listed = store.list_persisted(None)?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, "session-roundtrip");
    assert_eq!(listed[0].cwd, cwd);

    Ok(())
}

#[test_log::test]
fn delete_session_removes_persisted_record()
-> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let state_dir = std::env::temp_dir().join(format!("acp-llm-session-delete-{}", Uuid::new_v4()));
    let cwd = state_dir.join("workspace");
    let store = FilesystemSessionStore::new(&state_dir);
    let meta = PersistedSessionMeta {
        session_id: "session-delete".to_string(),
        cwd,
        additional_directories: vec![state_dir.join("extra")],
        mode: SessionBehavior::Ask,
        model: "deepseek-v4-pro".to_string(),
        reasoning_effort: ReasoningEffort::High,
        max_tokens: None,
        mcp_servers: Vec::new(),
        title: Some("delete me".to_string()),
        updated_at: Some("2026-06-14T00:00:00Z".to_string()),
        cost_micros: 0,
    };

    store.persist_turn(&meta, &[ChatMessage::user("hello")])?;
    assert!(store.delete_session("session-delete")?);
    assert!(store.load_record("session-delete").is_err());
    assert!(!store.delete_session("session-delete")?);

    Ok(())
}

#[test]
fn persisted_session_meta_deserializes_existing_modes()
-> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    for mode_id in ["ask", "accept-edits", "yolo"] {
        let meta: PersistedSessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": "session-legacy",
            "cwd": "/tmp/workspace",
            "additional_directories": [],
            "mode": mode_id,
            "model": "deepseek-v4-pro",
            "reasoning_effort": "high",
            "max_tokens": null,
            "mcp_servers": [],
            "title": null,
            "updated_at": null,
        }))?;
        assert_eq!(meta.mode.mode_id(), mode_id);
    }

    Ok(())
}

#[test_log::test]
fn rejects_session_ids_that_are_not_path_components() {
    let store = FilesystemSessionStore::new("/tmp/acp-llm-invalid");
    let error = store.load_record("../escape").err();
    assert!(error.is_some());
}

#[test_log::test]
fn proxy_logs_never_surface_as_resumable_sessions()
-> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let state_dir = std::env::temp_dir().join(format!("acp-llm-proxy-guard-{}", Uuid::new_v4()));
    let store = FilesystemSessionStore::new(&state_dir);
    let meta = PersistedSessionMeta {
        session_id: "session-mine".to_string(),
        cwd: state_dir.join("workspace"),
        additional_directories: Vec::new(),
        mode: SessionBehavior::Plan,
        model: "deepseek-v4-pro".to_string(),
        reasoning_effort: ReasoningEffort::Max,
        max_tokens: None,
        mcp_servers: Vec::new(),
        title: None,
        updated_at: None,
        cost_micros: 0,
    };
    store.persist_turn(&meta, &[ChatMessage::user("hello")])?;

    // A proxied foreign session, written where the proxy actually writes it:
    // under the proxy root, not the directory the store enumerates.
    let proxied = state_dir
        .join(acp_llm_adapter::paths::PROXY_DIR)
        .join("sessions")
        .join("session-someone-elses");
    std::fs::create_dir_all(&proxied)?;
    std::fs::write(proxied.join("log.jsonl"), "{\"kind\":\"frame\"}\n")?;

    let listed = store.list_persisted(None)?;

    assert_eq!(
        listed.len(),
        1,
        "a proxied agent's session is not an adapter session and must not be offered for resume"
    );
    assert_eq!(listed[0].session_id, "session-mine");

    let _ = std::fs::remove_dir_all(&state_dir);
    Ok(())
}
