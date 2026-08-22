use super::{KIND_STDERR, LineSplitter, record_for};
use crate::logsink::{Direction, KIND_FRAME};

fn lines_as_strings(lines: Vec<Vec<u8>>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| String::from_utf8_lossy(&line).into_owned())
        .collect()
}

#[test]
fn a_whole_chunk_of_lines_is_split() {
    let mut splitter = LineSplitter::default();

    let lines = splitter.push(b"one\ntwo\nthree\n");

    assert_eq!(lines_as_strings(lines), vec!["one", "two", "three"]);
    assert_eq!(splitter.finish(), None);
}

#[test]
fn a_line_split_across_reads_is_reassembled() {
    let mut splitter = LineSplitter::default();

    let first = splitter.push(br#"{"jsonrpc":"2.0","#);
    let second = splitter.push(br#""id":1}"#);
    let third = splitter.push(b"\n");

    assert!(first.is_empty(), "no line is complete yet");
    assert!(second.is_empty(), "still no newline seen");
    assert_eq!(
        lines_as_strings(third),
        vec![r#"{"jsonrpc":"2.0","id":1}"#],
        "the line is reassembled once its newline arrives"
    );
}

#[test]
fn a_newline_landing_alone_in_a_read_still_terminates_the_line() {
    let mut splitter = LineSplitter::default();

    splitter.push(b"partial");
    let lines = splitter.push(b"\nnext");

    assert_eq!(lines_as_strings(lines), vec!["partial"]);
    assert_eq!(
        splitter
            .finish()
            .map(|tail| String::from_utf8_lossy(&tail).into_owned()),
        Some("next".to_string())
    );
}

#[test]
fn an_empty_line_is_preserved() {
    let mut splitter = LineSplitter::default();

    let lines = splitter.push(b"a\n\nb\n");

    assert_eq!(lines_as_strings(lines), vec!["a", "", "b"]);
}

#[test]
fn unterminated_trailing_bytes_surface_at_finish() {
    let mut splitter = LineSplitter::default();

    let lines = splitter.push(b"complete\nhalf");

    assert_eq!(lines_as_strings(lines), vec!["complete"]);
    assert_eq!(
        splitter
            .finish()
            .map(|tail| String::from_utf8_lossy(&tail).into_owned()),
        Some("half".to_string()),
        "a crashed agent's last partial line is still worth logging"
    );
    assert_eq!(splitter.finish(), None, "finish drains the buffer");
}

#[test]
fn a_frame_line_becomes_structured_json() {
    let record = record_for(
        Direction::AgentToClient,
        KIND_FRAME,
        br#"{"jsonrpc":"2.0","id":7}"#,
    );

    assert_eq!(
        record.payload.get("id").and_then(serde_json::Value::as_u64),
        Some(7)
    );
}

#[test]
fn a_stderr_line_stays_plain_text() {
    let record = record_for(Direction::Internal, KIND_STDERR, b"warning: something");

    assert_eq!(
        record.payload.as_str(),
        Some("warning: something"),
        "stderr is not JSON and must not be coerced into it"
    );
    assert_eq!(record.kind, KIND_STDERR);
}

#[test]
fn invalid_utf8_does_not_lose_the_line() {
    let record = record_for(Direction::Internal, KIND_STDERR, &[0xff, 0xfe, b'o', b'k']);

    assert!(
        record
            .payload
            .as_str()
            .is_some_and(|text| text.ends_with("ok")),
        "undecodable bytes are replaced, not dropped"
    );
}

// ── session sniffing ────────────────────────────────────────

use super::{METHOD_SESSION_LOAD, METHOD_SESSION_NEW, SessionSniffer, Sniffed};
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
