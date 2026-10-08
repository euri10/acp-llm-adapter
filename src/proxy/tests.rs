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

    assert!(
        record.payload.is_string(),
        "stderr stays a string record even when redacted"
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
            .is_some_and(|text| text == "[REDACTED]" || text == "\u{fffd}\u{fffd}ok"),
        "undecodable text is redacted or decoded with replacement characters"
    );
}
