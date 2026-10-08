// ── session sniffing ────────────────────────────────────────

use super::{METHOD_SESSION_LOAD, METHOD_SESSION_NEW, SessionSniffer, Sniffed};
use crate::logsink::Direction;
use serde_json::json;

#[test]
fn a_session_new_response_is_matched_to_its_request() {
    let mut sniffer = SessionSniffer::default();

    let request = sniffer.observe(
        Direction::ClientToAgent,
        &json!({"jsonrpc": "2.0", "id": 2, "method": METHOD_SESSION_NEW, "params": {}}),
    );
    let response = sniffer.observe(
        Direction::AgentToClient,
        &json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "session-abc"}}),
    );

    assert_eq!(
        request,
        Sniffed::default(),
        "the request creates nothing yet"
    );
    assert_eq!(
        response,
        Sniffed {
            session_id: Some("session-abc".to_string()),
            established: Some("session-abc".to_string()),
        },
        "a response carries no method name, so only the id correlation finds it"
    );
}

#[test]
fn an_unrelated_response_with_the_same_shape_is_ignored() {
    let mut sniffer = SessionSniffer::default();

    // No session/new request was ever seen for id 9.
    let observed = sniffer.observe(
        Direction::AgentToClient,
        &json!({"jsonrpc": "2.0", "id": 9, "result": {"sessionId": "session-not-ours"}}),
    );

    assert_eq!(observed, Sniffed::default());
}

#[test]
fn a_correlated_response_is_only_matched_once() {
    let mut sniffer = SessionSniffer::default();
    sniffer.observe(
        Direction::ClientToAgent,
        &json!({"id": 2, "method": METHOD_SESSION_NEW}),
    );

    let first = sniffer.observe(
        Direction::AgentToClient,
        &json!({"id": 2, "result": {"sessionId": "session-abc"}}),
    );
    let second = sniffer.observe(
        Direction::AgentToClient,
        &json!({"id": 2, "result": {"sessionId": "session-abc"}}),
    );

    assert!(first.established.is_some());
    assert_eq!(second, Sniffed::default(), "the correlation is consumed");
}

#[test]
fn string_request_ids_correlate_too() {
    let mut sniffer = SessionSniffer::default();
    sniffer.observe(
        Direction::ClientToAgent,
        &json!({"id": "req-1", "method": METHOD_SESSION_NEW}),
    );

    let observed = sniffer.observe(
        Direction::AgentToClient,
        &json!({"id": "req-1", "result": {"sessionId": "session-abc"}}),
    );

    assert_eq!(observed.established, Some("session-abc".to_string()));
}

#[test]
fn a_notification_is_attributed_by_its_own_session_id() {
    let mut sniffer = SessionSniffer::default();

    let observed = sniffer.observe(
        Direction::AgentToClient,
        &json!({"method": "session/update", "params": {"sessionId": "session-xyz"}}),
    );

    assert_eq!(
        observed,
        Sniffed {
            session_id: Some("session-xyz".to_string()),
            established: None,
        },
        "notifications name their own session, so one process serving several \
         sessions keeps them in separate files"
    );
}

#[test]
fn loading_a_session_binds_without_waiting_for_a_response() {
    let mut sniffer = SessionSniffer::default();

    let observed = sniffer.observe(
        Direction::ClientToAgent,
        &json!({"id": 3, "method": METHOD_SESSION_LOAD, "params": {"sessionId": "session-old"}}),
    );

    assert_eq!(observed.established, Some("session-old".to_string()));
}

#[test]
fn a_frame_that_is_not_json_reveals_nothing_and_does_not_panic() {
    let mut sniffer = SessionSniffer::default();

    let observed = sniffer.observe(
        Direction::AgentToClient,
        &serde_json::Value::String("garbage on the wire".to_string()),
    );

    assert_eq!(observed, Sniffed::default());
}

#[test]
fn stderr_is_never_sniffed_for_sessions() {
    let mut sniffer = SessionSniffer::default();

    let observed = sniffer.observe(
        Direction::Internal,
        &json!({"result": {"sessionId": "session-abc"}}),
    );

    assert_eq!(observed, Sniffed::default());
}

#[test]
fn session_replies_keep_request_direction_and_id_type_separate() {
    let mut sniffer = SessionSniffer::default();
    for (direction, id, session) in [
        (Direction::ClientToAgent, json!(7), "client-number"),
        (Direction::ClientToAgent, json!("7"), "client-string"),
        (Direction::AgentToClient, json!(7), "agent-number"),
    ] {
        sniffer.observe(
            direction,
            &json!({
                "id":id, "method":"session/request", "params":{"sessionId":session}
            }),
        );
    }
    for (direction, id, session) in [
        (Direction::AgentToClient, json!("7"), "client-string"),
        (Direction::ClientToAgent, json!(7), "agent-number"),
        (Direction::AgentToClient, json!(7), "client-number"),
    ] {
        let response = json!({"id":id, "error":{"code":-32600,"message":"failed"}});
        assert_eq!(
            sniffer.observe(direction, &response).session_id.as_deref(),
            Some(session)
        );
        assert_eq!(sniffer.observe(direction, &response), Sniffed::default());
    }
}

#[test]
fn agent_request_cannot_consume_a_client_session_creation() {
    let mut sniffer = SessionSniffer::default();
    sniffer.observe(
        Direction::ClientToAgent,
        &json!({"id":1,"method":"session/new"}),
    );
    assert_eq!(
        sniffer
            .observe(
                Direction::AgentToClient,
                &json!({
                    "id":1,"method":"session/request_permission","params":{"sessionId":"existing"}
                })
            )
            .session_id
            .as_deref(),
        Some("existing")
    );
    assert_eq!(
        sniffer
            .observe(
                Direction::AgentToClient,
                &json!({
                    "id":1,"result":{"sessionId":"created"}
                })
            )
            .session_id
            .as_deref(),
        Some("created")
    );
    assert_eq!(
        sniffer
            .observe(
                Direction::ClientToAgent,
                &json!({
                    "id":1,"result":{}
                })
            )
            .session_id
            .as_deref(),
        Some("existing")
    );
}
