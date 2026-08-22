use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ConnectionLog, Direction, ENV_UNREDACTED, KIND_FRAME, KIND_SESSION_BOUND, LogRecord, LogSink,
    LogSinkError, LogWriter, RetentionPolicy, redaction_enabled_fn,
};

/// A temp root that removes itself, so tests do not litter the state directory.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("acp-logsink-{label}-{}", Uuid::new_v4()));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Drop every producer handle, then drain the writer inline.
///
/// Running the writer on the test thread keeps these tests deterministic —
/// no sleeps, no polling for a background thread to catch up.
fn drain(sink: std::sync::Arc<LogSink>, connection: ConnectionLog, writer: LogWriter) {
    drop(connection);
    drop(sink);
    writer.run();
}

fn drain_writer(sink: std::sync::Arc<LogSink>, writer: LogWriter) {
    drop(sink);
    writer.run();
}

/// Open a connection handle for a test-controlled identifier.
///
/// Every call site passes a literal that `validate_id` accepts, so the error
/// arm is genuinely unreachable rather than an unwrap in disguise.
fn open(sink: &std::sync::Arc<LogSink>, id: &str) -> ConnectionLog {
    sink.connection(id)
        .unwrap_or_else(|error| unreachable!("test identifier {id} rejected: {error}"))
}

fn read_records(path: &Path) -> Vec<LogRecord> {
    let contents = fs::read_to_string(path).unwrap_or_default();
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn record_round_trips_through_the_envelope() {
    let record = LogRecord::new(
        Direction::ClientToAgent,
        KIND_FRAME,
        json!({"jsonrpc": "2.0", "id": 1}),
    )
    .with_session("session-abc");

    let encoded = serde_json::to_string(&record).unwrap_or_default();
    let decoded: LogRecord = serde_json::from_str(&encoded)
        .unwrap_or_else(|_| LogRecord::new(Direction::Internal, "decode-failed", Value::Null));

    assert_eq!(decoded, record);
}

#[test]
fn record_serialises_to_exactly_one_line() {
    let record = LogRecord::frame(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","result":{"nested":{"deep":true}}}"#,
    );

    let encoded = serde_json::to_string(&record).unwrap_or_default();

    assert!(!encoded.contains('\n'), "record must not embed newlines");
}

#[test]
fn unparseable_frame_is_kept_verbatim_as_a_string() {
    let record = LogRecord::frame(Direction::AgentToClient, "not json at all");

    assert_eq!(record.payload, Value::String("not json at all".to_string()));
    assert_eq!(record.kind, KIND_FRAME);
}

#[test]
fn structured_payloads_share_one_redaction_policy() {
    let secret = "prompt and tool secret";
    let payload = json!({
        "method": "session/prompt",
        "params": {
            "prompt": [{"text": secret}],
            "arguments": secret,
            "model": "safe-metadata"
        }
    });

    let frame = LogRecord::new(Direction::ClientToAgent, KIND_FRAME, payload.clone());
    let event = LogRecord::new(Direction::Internal, "trace-event", payload);

    assert!(!frame.payload.to_string().contains(secret));
    assert!(!event.payload.to_string().contains(secret));
    let params = frame.payload.get("params").and_then(Value::as_object);
    assert_eq!(
        params.and_then(|params| params.get("model")),
        Some(&json!("safe-metadata"))
    );
    assert_eq!(
        params.and_then(|params| params.get("prompt")),
        Some(&json!("[REDACTED]"))
    );
}

#[test]
fn unredacted_env_disables_the_shared_redaction_policy() {
    let secret = "prompt and tool secret";
    let payload = json!({
        "method": "session/prompt",
        "params": {"prompt": [{"text": secret}], "arguments": secret}
    });

    let record =
        LogRecord::new_with_redaction(Direction::ClientToAgent, KIND_FRAME, payload, false);

    assert!(record.payload.to_string().contains(secret));
}

#[test]
fn redaction_enabled_fn_defaults_to_redacting() {
    assert!(redaction_enabled_fn(|_| None));
}

#[test]
fn redaction_enabled_fn_treats_unset_and_zero_as_redact() {
    assert!(redaction_enabled_fn(
        |key| (key == ENV_UNREDACTED).then(|| "0".to_string())
    ));
    assert!(redaction_enabled_fn(
        |key| (key == ENV_UNREDACTED).then(String::new)
    ));
}

#[test]
fn redaction_enabled_fn_treats_any_other_value_as_opt_out() {
    assert!(!redaction_enabled_fn(
        |key| (key == ENV_UNREDACTED).then(|| "1".to_string())
    ));
    assert!(!redaction_enabled_fn(
        |key| (key == ENV_UNREDACTED).then(|| "true".to_string())
    ));
}

#[test]
fn absent_session_id_is_omitted_from_the_encoding() {
    let record = LogRecord::text(Direction::Internal, "note", "hello");

    let encoded = serde_json::to_string(&record).unwrap_or_default();

    assert!(
        !encoded.contains("session_id"),
        "unexpected session_id in {encoded}"
    );
}

#[test]
fn records_without_a_session_land_in_the_connection_file() {
    let root = TempRoot::new("connection");
    let (sink, writer) = LogSink::channel(root.path(), 16);
    let connection = open(&sink, "conn-1");

    connection.log(LogRecord::frame(
        Direction::ClientToAgent,
        r#"{"method":"initialize"}"#,
    ));

    let path = sink.connection_log_path("conn-1").unwrap_or_default();
    drain(sink, connection, writer);

    let records = read_records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(records.first().map(|r| r.session_id.clone()), Some(None));
}

#[test]
fn binding_a_session_reroutes_subsequent_records() {
    let root = TempRoot::new("reroute");
    let (sink, writer) = LogSink::channel(root.path(), 16);
    let connection = open(&sink, "conn-2");

    connection.log(LogRecord::frame(
        Direction::ClientToAgent,
        r#"{"method":"initialize"}"#,
    ));
    assert!(
        connection.bind_session("session-xyz").is_ok(),
        "binding a valid session id must succeed"
    );
    connection.log(LogRecord::frame(
        Direction::ClientToAgent,
        r#"{"method":"session/prompt"}"#,
    ));

    let connection_path = sink.connection_log_path("conn-2").unwrap_or_default();
    let session_path = sink.session_log_path("session-xyz").unwrap_or_default();
    drain(sink, connection, writer);

    let session_records = read_records(&session_path);
    assert_eq!(
        session_records.len(),
        1,
        "only the post-bind record belongs to the session"
    );
    assert_eq!(
        session_records.first().map(|r| r.session_id.clone()),
        Some(Some("session-xyz".to_string())),
        "the connection's session id is stamped onto its records"
    );

    let connection_records = read_records(&connection_path);
    assert_eq!(
        connection_records.len(),
        2,
        "the pre-bind record plus the mapping record"
    );
    assert_eq!(
        connection_records.get(1).map(|r| r.kind.clone()),
        Some(KIND_SESSION_BOUND.to_string()),
        "the mapping from connection to session is recorded"
    );
}

#[test]
fn session_logs_live_beside_the_session_store_layout() {
    let root = TempRoot::new("layout");
    let (sink, _writer) = LogSink::channel(root.path(), 4);

    let path = sink.session_log_path("session-abc").unwrap_or_default();

    assert_eq!(path, root.path().join("sessions/session-abc/log.jsonl"));
}

#[test]
fn two_sessions_do_not_share_a_file() {
    let root = TempRoot::new("isolation");
    let (sink, _writer) = LogSink::channel(root.path(), 4);

    let first = sink.session_log_path("session-aaa").unwrap_or_default();
    let second = sink.session_log_path("session-bbb").unwrap_or_default();

    assert_ne!(first, second);
}

#[test]
fn a_saturated_queue_drops_records_instead_of_blocking() {
    let root = TempRoot::new("saturated");
    // No writer is ever run, so nothing drains the queue: every send past
    // capacity must be dropped rather than parking the caller forever.
    let (sink, _writer) = LogSink::channel(root.path(), 1);
    let connection = open(&sink, "conn-3");

    for index in 0..5 {
        connection.log(LogRecord::text(
            Direction::Internal,
            "note",
            format!("{index}"),
        ));
    }

    assert_eq!(
        sink.dropped(),
        4,
        "one record fits the queue, the other four are dropped"
    );
    assert_eq!(sink.write_errors(), 0);
}

#[test]
fn a_dropped_writer_does_not_wedge_the_producer() {
    let root = TempRoot::new("disconnected");
    let (sink, writer) = LogSink::channel(root.path(), 4);
    let connection = open(&sink, "conn-4");
    drop(writer);

    connection.log(LogRecord::text(Direction::Internal, "note", "after"));

    assert_eq!(sink.dropped(), 1);
}

#[test]
fn flush_returns_when_the_writer_is_gone() {
    let root = TempRoot::new("flush");
    let (sink, writer) = LogSink::channel(root.path(), 4);
    drop(writer);

    // A hang here fails the test by timing out rather than by assertion.
    sink.flush();
}

#[test]
fn traversal_in_an_identifier_is_rejected() {
    let root = TempRoot::new("traversal");
    let (sink, _writer) = LogSink::channel(root.path(), 4);

    for bad in ["../escape", "with/slash", "", "dot.dot"] {
        assert!(
            matches!(sink.session_log_path(bad), Err(LogSinkError::InvalidId(_))),
            "identifier {bad:?} should be rejected"
        );
        assert!(
            matches!(sink.connection(bad), Err(LogSinkError::InvalidId(_))),
            "identifier {bad:?} should be rejected"
        );
    }
}

#[test]
fn binding_an_invalid_session_id_leaves_the_connection_unbound() {
    let root = TempRoot::new("bad-bind");
    let (sink, _writer) = LogSink::channel(root.path(), 4);
    let connection = open(&sink, "conn-5");

    let result = connection.bind_session("../escape");

    assert!(matches!(result, Err(LogSinkError::InvalidId(_))));
    assert_eq!(connection.session_id(), None);
}

#[test]
fn a_separate_root_keeps_proxy_logs_out_of_the_session_store() {
    let root = TempRoot::new("roots");
    let store_root = root.path().join("state");
    let proxy_root = root.path().join("state/proxy");

    let (store_sink, _store_writer) = LogSink::channel(&store_root, 4);
    let (proxy_sink, _proxy_writer) = LogSink::channel(&proxy_root, 4);

    let store_path = store_sink
        .session_log_path("session-aaa")
        .unwrap_or_default();
    let proxy_path = proxy_sink
        .session_log_path("session-aaa")
        .unwrap_or_default();

    assert_ne!(
        store_path, proxy_path,
        "the same session id under two roots must not collide"
    );
    assert!(
        !proxy_path.starts_with(store_root.join("sessions")),
        "proxy sessions must not land in the directory the session store enumerates"
    );
}

#[test]
fn a_migrated_legacy_record_deserialises() {
    // Emitted by the one-time legacy log migration. The script wrote this
    // shape by hand, so the envelope contract has to be asserted somewhere.
    let line = r#"{"timestamp": "2026-08-20T14:32:15.000Z", "direction": "agent_to_client", "kind": "frame", "session_id": "session-36d6f05c-0022-4401-89cb-315ccf1f8467", "payload": {"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "session-36d6f05c-0022-4401-89cb-315ccf1f8467"}}}"#;

    let decoded: LogRecord = match serde_json::from_str(line) {
        Ok(decoded) => decoded,
        Err(error) => unreachable!("migrated record rejected: {error}"),
    };

    assert_eq!(decoded.direction, Direction::AgentToClient);
    assert_eq!(decoded.kind, KIND_FRAME);
    assert_eq!(
        decoded.session_id.as_deref(),
        Some("session-36d6f05c-0022-4401-89cb-315ccf1f8467"),
        "migrated records keep the session they were routed to"
    );
    assert!(decoded.timestamp.ends_with('Z'));
}

#[test]
fn retention_bounds_apply_on_startup_and_preserve_session_artifacts() {
    let root = TempRoot::new("retention");
    let old_log = root.path().join("connections/old.jsonl");
    let session_log = root.path().join("sessions/session-1/log.jsonl");
    let metadata = root.path().join("sessions/session-1/meta.json");
    let history = root.path().join("sessions/session-1/history.jsonl");
    for path in [&old_log, &session_log, &metadata, &history] {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_default();
        }
        fs::write(path, "seed").unwrap_or_default();
    }

    let policy = RetentionPolicy {
        max_bytes: u64::MAX,
        max_age: Duration::ZERO,
    };
    let (sink, writer) = LogSink::channel_with_policy(root.path(), 4, policy);
    drain_writer(sink, writer);

    assert!(!old_log.exists());
    assert!(!session_log.exists());
    assert!(
        metadata.exists(),
        "retention must not remove session metadata"
    );
    assert!(
        history.exists(),
        "retention must not remove session history"
    );
}

#[test]
fn size_bound_is_rechecked_after_a_restart() {
    let root = TempRoot::new("size-restart");
    let policy = RetentionPolicy {
        max_bytes: 1,
        max_age: Duration::from_hours(30 * 24),
    };
    let (sink, writer) = LogSink::channel_with_policy(root.path(), 4, policy);
    let connection = open(&sink, "conn-size");
    connection.log(LogRecord::text(
        Direction::Internal,
        "large-record",
        "this exceeds the one byte bound",
    ));
    drain(sink, connection, writer);

    let (sink, writer) = LogSink::channel_with_policy(root.path(), 4, policy);
    drain_writer(sink, writer);

    let total = ["connections", "sessions"]
        .iter()
        .map(|directory| directory_size(&root.path().join(directory)))
        .sum::<u64>();
    assert!(total <= policy.max_bytes);
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                directory_size(&path)
            } else {
                fs::metadata(path).map_or(0, |metadata| metadata.len())
            }
        })
        .sum()
}
