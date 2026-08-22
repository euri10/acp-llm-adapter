//! Shared NDJSON log sink.
//!
//! Both the ACP adapter and the ACP proxy write their logs through this sink so
//! the two cannot drift into different formats. A single reader can then parse
//! either, regardless of which produced the file.
//!
//! # Layout
//!
//! Every destination lives under a caller-supplied root:
//!
//! ```text
//! <root>/connections/<connection-id>.jsonl   — records with no session yet
//! <root>/sessions/<session-id>/log.jsonl     — records belonging to a session
//! ```
//!
//! The adapter roots the sink at its state directory, so a session's log lands
//! beside that session's persisted metadata and history. The proxy roots the
//! sink somewhere else entirely, which is what keeps proxied foreign sessions
//! out of the adapter's own session store.
//!
//! # Ordering and the connection-scoped file
//!
//! A session id does not exist until the agent answers `session/new`, so every
//! connection starts writing to a connection-scoped file and switches to the
//! session file once [`crate::logsink::ConnectionLog::bind_session`] is called. The switch
//! leaves a mapping record behind in the connection file, so the two halves of
//! a session's story can always be stitched back together.
//!
//! # Backpressure
//!
//! Writing never blocks the caller. Records go onto a bounded queue drained by
//! a writer, and a full queue drops records and counts them rather than
//! stalling. This is not an optimisation: in proxy mode a blocked sink would
//! stall the wrapped agent, which is worse than losing log lines.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::timestamp::iso_timestamp_millis_now;

const CONNECTIONS_DIR: &str = "connections";
const SESSIONS_DIR: &str = "sessions";
const SESSION_LOG_FILE: &str = "log.jsonl";

/// Record kind emitted when a connection learns its session id.
pub const KIND_SESSION_BOUND: &str = "session-bound";
/// Record kind for a raw JSON-RPC frame crossing the wire.
pub const KIND_FRAME: &str = "frame";
/// Record kind for an internal tracing event.
pub const KIND_TRACE: &str = "trace-event";

/// Environment variable overriding the aggregate log size limit.
pub const ENV_MAX_BYTES: &str = "ACP_LOG_MAX_BYTES";
/// Environment variable overriding the log age limit in days.
pub const ENV_MAX_AGE_DAYS: &str = "ACP_LOG_MAX_AGE_DAYS";
/// Environment variable disabling redaction of sensitive log fields.
///
/// Off by default: logs redact prompts, messages, and tool arguments. Set to
/// any value other than empty or `0` to write those fields in the clear, for
/// local debugging where the real content is what you're chasing.
pub const ENV_UNREDACTED: &str = "ACP_LOG_UNREDACTED";
/// Default aggregate size limit for structured logs.
pub const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Default age limit for structured logs.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_hours(30 * 24);

/// Retention bounds applied to log files only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Maximum combined size of all connection and session log files.
    pub max_bytes: u64,
    /// Maximum age of a log file before it is removed.
    pub max_age: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_age: DEFAULT_MAX_AGE,
        }
    }
}

impl RetentionPolicy {
    /// Read retention overrides, falling back to documented defaults when an
    /// override is missing, zero, or malformed.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let max_bytes = std::env::var(ENV_MAX_BYTES)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(defaults.max_bytes);
        let max_age_days = std::env::var(ENV_MAX_AGE_DAYS)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value: &u64| *value > 0)
            .unwrap_or(defaults.max_age.as_secs() / (24 * 60 * 60));
        let max_age = max_age_days
            .checked_mul(24 * 60 * 60)
            .map_or(defaults.max_age, Duration::from_secs);

        Self { max_bytes, max_age }
    }
}

/// Error returned when configuring or writing through the log sink.
#[derive(Debug, Error)]
pub enum LogSinkError {
    /// The identifier cannot be represented as a safe path component.
    #[error("invalid log identifier: {0}")]
    InvalidId(String),
    /// Filesystem I/O failed.
    #[error("log sink I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding failed.
    #[error("log sink JSON encoding failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Which way a logged record was travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Sent by the ACP client towards the agent.
    ClientToAgent,
    /// Sent by the agent towards the ACP client.
    AgentToClient,
    /// Produced inside this process rather than observed on the wire.
    Internal,
}

/// One line of a log file.
///
/// Serialises to a single JSON object; a log file is these separated by
/// newlines. Both the adapter and the proxy emit this same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// ISO 8601 UTC timestamp with millisecond precision.
    pub timestamp: String,
    /// Session this record belongs to, absent while the session is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Direction of travel.
    pub direction: Direction,
    /// Record category, such as [`KIND_FRAME`] or [`KIND_SESSION_BOUND`].
    pub kind: String,
    /// Record body.
    pub payload: Value,
}

impl LogRecord {
    /// Build a record stamped with the current time and no session id.
    ///
    /// The session id is filled in by [`ConnectionLog::log`] when the
    /// connection already knows it, so callers rarely set it themselves.
    pub fn new(direction: Direction, kind: impl Into<String>, payload: Value) -> Self {
        Self::new_with_redaction(direction, kind, payload, redaction_enabled())
    }

    /// Build a record, choosing redaction explicitly rather than from the
    /// environment.
    ///
    /// This is the testable core of [`LogRecord::new`]; production code
    /// should use [`LogRecord::new`] directly.
    fn new_with_redaction(
        direction: Direction,
        kind: impl Into<String>,
        payload: Value,
        redact: bool,
    ) -> Self {
        Self {
            timestamp: iso_timestamp_millis_now(),
            session_id: None,
            direction,
            kind: kind.into(),
            payload: if redact {
                redact_value(payload)
            } else {
                payload
            },
        }
    }

    /// Build a record from a raw wire line.
    ///
    /// ACP over stdio is NDJSON, so a line is normally a JSON-RPC object and is
    /// stored as structured JSON. A line that does not parse is stored verbatim
    /// as a string rather than discarded — malformed traffic is exactly what a
    /// debugging log exists to capture.
    #[must_use]
    pub fn frame(direction: Direction, raw: &str) -> Self {
        let payload = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        Self::new(direction, KIND_FRAME, payload)
    }

    /// Build a record whose body is plain text, such as captured stderr.
    pub fn text(direction: Direction, kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(direction, kind, Value::String(text.into()))
    }

    /// Attach a session id to this record.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }
}

/// A file a record can be written to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Destination {
    /// The connection-scoped file used before a session id is known.
    Connection(String),
    /// A session's own log file.
    Session(String),
}

/// Filesystem layout for log destinations rooted at a directory.
#[derive(Debug, Clone)]
struct LogLayout {
    root: PathBuf,
}

impl LogLayout {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn connection_path(&self, connection_id: &str) -> PathBuf {
        self.root
            .join(CONNECTIONS_DIR)
            .join(format!("{connection_id}.jsonl"))
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(SESSIONS_DIR)
            .join(session_id)
            .join(SESSION_LOG_FILE)
    }

    fn path_for(&self, destination: &Destination) -> PathBuf {
        match destination {
            Destination::Connection(id) => self.connection_path(id),
            Destination::Session(id) => self.session_path(id),
        }
    }
}

/// Counters describing sink health.
#[derive(Debug, Default)]
struct Counters {
    dropped: AtomicU64,
    write_errors: AtomicU64,
}

/// A command handed to the writer.
enum Command {
    /// Append a record to a destination.
    Write(Destination, Box<LogRecord>),
    /// Acknowledge once every previously queued command has been handled.
    Flush(SyncSender<()>),
}

/// The producer half of the sink: cheap to clone, never blocks.
///
/// Create one with [`LogSink::spawn`] for normal use, or with
/// [`LogSink::channel`] when the caller wants to drive the writer itself.
#[derive(Debug)]
pub struct LogSink {
    sender: SyncSender<Command>,
    layout: LogLayout,
    counters: Arc<Counters>,
}

impl LogSink {
    /// Build a sink and its writer without starting a thread.
    ///
    /// The caller is responsible for running [`LogWriter::run`]. `capacity` is
    /// the number of records that may be queued before further records are
    /// dropped.
    pub fn channel(root: impl Into<PathBuf>, capacity: usize) -> (Arc<Self>, LogWriter) {
        Self::channel_with_policy(root, capacity, RetentionPolicy::default())
    }

    /// Build a sink and its writer with explicit retention bounds.
    pub fn channel_with_policy(
        root: impl Into<PathBuf>,
        capacity: usize,
        policy: RetentionPolicy,
    ) -> (Arc<Self>, LogWriter) {
        let (sender, receiver) = sync_channel(capacity);
        let layout = LogLayout::new(root);
        let counters = Arc::new(Counters::default());

        let sink = Arc::new(Self {
            sender,
            layout: layout.clone(),
            counters: Arc::clone(&counters),
        });
        let writer = LogWriter {
            receiver,
            layout,
            counters,
            files: HashMap::new(),
            policy,
        };

        (sink, writer)
    }

    /// Build a sink and run its writer on a dedicated thread.
    ///
    /// The writer stops once every sink handle has been dropped. Records still
    /// queued when the process exits are lost, so call [`LogSink::flush`] on
    /// any orderly shutdown path.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::Io`] if the writer thread cannot be spawned.
    pub fn spawn(root: impl Into<PathBuf>, capacity: usize) -> Result<Arc<Self>, LogSinkError> {
        Self::spawn_with_policy(root, capacity, RetentionPolicy::from_env())
    }

    /// Build a sink with explicit retention bounds and run its writer thread.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::Io`] if the writer thread cannot be spawned.
    pub fn spawn_with_policy(
        root: impl Into<PathBuf>,
        capacity: usize,
        policy: RetentionPolicy,
    ) -> Result<Arc<Self>, LogSinkError> {
        let (sink, writer) = Self::channel_with_policy(root, capacity, policy);
        std::thread::Builder::new()
            .name("acp-log-writer".to_string())
            .spawn(move || writer.run())?;
        Ok(sink)
    }

    /// Open a connection-scoped logging handle.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::InvalidId`] if `connection_id` is not a safe
    /// path component.
    pub fn connection(
        self: &Arc<Self>,
        connection_id: impl Into<String>,
    ) -> Result<ConnectionLog, LogSinkError> {
        let connection_id = connection_id.into();
        validate_id(&connection_id)?;

        Ok(ConnectionLog {
            sink: Arc::clone(self),
            connection_id,
            session_id: Arc::new(RwLock::new(None)),
        })
    }

    /// Absolute path of a session's log file.
    ///
    /// The file exists only once that session has produced a record.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::InvalidId`] if `session_id` is not a safe path
    /// component.
    pub fn session_log_path(&self, session_id: &str) -> Result<PathBuf, LogSinkError> {
        validate_id(session_id)?;
        Ok(self.layout.session_path(session_id))
    }

    /// Absolute path of a connection's log file.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::InvalidId`] if `connection_id` is not a safe
    /// path component.
    pub fn connection_log_path(&self, connection_id: &str) -> Result<PathBuf, LogSinkError> {
        validate_id(connection_id)?;
        Ok(self.layout.connection_path(connection_id))
    }

    /// Number of records dropped because the queue was full or the writer gone.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    /// Number of records the writer failed to write.
    #[must_use]
    pub fn write_errors(&self) -> u64 {
        self.counters.write_errors.load(Ordering::Relaxed)
    }

    /// Block until every record queued so far has been written.
    ///
    /// Returns once the writer acknowledges, or immediately if the writer has
    /// already stopped.
    pub fn flush(&self) {
        let (ack, wait) = sync_channel(0);
        if self.sender.send(Command::Flush(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// Queue a record, dropping it rather than blocking if the queue is full.
    fn enqueue(&self, destination: Destination, record: LogRecord) {
        match self
            .sender
            .try_send(Command::Write(destination, Box::new(record)))
        {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The consumer half of the sink: owns the open files and does the writing.
#[derive(Debug)]
pub struct LogWriter {
    receiver: Receiver<Command>,
    layout: LogLayout,
    counters: Arc<Counters>,
    files: HashMap<Destination, File>,
    policy: RetentionPolicy,
}

impl LogWriter {
    /// Drain commands until every sink handle has been dropped.
    pub fn run(mut self) {
        self.prune_and_report();
        while let Ok(command) = self.receiver.recv() {
            match command {
                Command::Write(destination, record) => self.write(&destination, &record),
                Command::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
    }

    fn write(&mut self, destination: &Destination, record: &LogRecord) {
        if let Err(error) = self.try_write(destination, record) {
            self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
            // The sink cannot log its own failure through itself, and stderr is
            // the adapter's existing diagnostic channel.
            tracing::warn!(%error, "log sink write failed");
        }
    }

    fn prune_and_report(&mut self) {
        if let Err(error) = self.prune() {
            self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%error, "log sink retention sweep failed");
        }
    }

    fn try_write(
        &mut self,
        destination: &Destination,
        record: &LogRecord,
    ) -> Result<(), LogSinkError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');

        if !self.files.contains_key(destination) {
            let path = self.layout.path_for(destination);
            let file = open_append(&path)?;
            self.files.insert(destination.clone(), file);
        }
        // The entry was just inserted when absent, so this lookup always hits;
        // the fallback avoids a panic rather than expressing a real case.
        let Some(file) = self.files.get_mut(destination) else {
            return Ok(());
        };

        // Records are written unbuffered: a debugging log that loses its last
        // lines to a crash is useless precisely when it is needed.
        file.write_all(&line)?;
        self.prune()?;
        Ok(())
    }

    fn prune(&mut self) -> Result<(), LogSinkError> {
        let now = SystemTime::now();
        let cutoff = now.checked_sub(self.policy.max_age).unwrap_or(UNIX_EPOCH);
        let mut files = self.log_files()?;

        for file in files.iter().filter(|file| file.modified < cutoff) {
            self.remove_log_file(&file.path)?;
        }

        files = self.log_files()?;
        let mut total = files.iter().map(|file| file.size).sum::<u64>();
        files.sort_by_key(|file| file.modified);
        for file in files {
            if total <= self.policy.max_bytes {
                break;
            }
            total = total.saturating_sub(file.size);
            self.remove_log_file(&file.path)?;
        }
        Ok(())
    }

    fn log_files(&self) -> Result<Vec<LogFile>, LogSinkError> {
        let mut files = Vec::new();
        collect_log_files(&self.layout.root.join(CONNECTIONS_DIR), false, &mut files)?;
        collect_log_files(&self.layout.root.join(SESSIONS_DIR), true, &mut files)?;
        Ok(files)
    }

    fn remove_log_file(&mut self, path: &Path) -> Result<(), LogSinkError> {
        let destinations = self
            .files
            .keys()
            .filter(|destination| self.layout.path_for(destination) == path)
            .cloned()
            .collect::<Vec<_>>();
        for destination in destinations {
            self.files.remove(&destination);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Debug)]
struct LogFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn collect_log_files(
    directory: &Path,
    session_logs_only: bool,
    files: &mut Vec<LogFile>,
) -> Result<(), LogSinkError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_log_files(&path, session_logs_only, files)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && (!session_logs_only
                || path
                    .file_name()
                    .is_some_and(|name| name == SESSION_LOG_FILE))
        {
            let metadata = fs::metadata(&path)?;
            files.push(LogFile {
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    Ok(())
}

/// Whether records should be redacted before being written.
///
/// Read fresh on every call rather than cached: `ACP_LOG_MAX_BYTES` and
/// `ACP_LOG_MAX_AGE_DAYS` are resolved once at sink startup because they
/// configure a background writer thread, but redaction happens inline at
/// record construction, far from any sink, so there is no natural place to
/// cache it — and `var_os` is a process-table lookup, not a syscall.
fn redaction_enabled() -> bool {
    redaction_enabled_fn(|key| std::env::var_os(key).and_then(|value| value.into_string().ok()))
}

/// Testable core of [`redaction_enabled`], injected with a lookup function so
/// tests do not need to mutate real process environment, which is unsound
/// under parallel test execution.
fn redaction_enabled_fn(mut lookup: impl FnMut(&str) -> Option<String>) -> bool {
    !lookup(ENV_UNREDACTED).is_some_and(|value| !value.is_empty() && value != "0")
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(redact_value).collect()),
        Value::Object(mut object) => {
            for (key, value) in &mut object {
                if is_sensitive_key(key) {
                    *value = Value::String("[REDACTED]".to_string());
                } else {
                    let current = std::mem::take(value);
                    *value = redact_value(current);
                }
            }
            Value::Object(object)
        }
        value => value,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key,
        "arguments" | "content" | "input" | "messages" | "prompt" | "text"
    )
}

fn open_append(path: &Path) -> Result<File, LogSinkError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

/// A logging handle for one client connection.
///
/// Records go to a connection-scoped file until [`ConnectionLog::bind_session`]
/// is called, and to that session's file afterwards.
#[derive(Debug, Clone)]
pub struct ConnectionLog {
    sink: Arc<LogSink>,
    connection_id: String,
    session_id: Arc<RwLock<Option<String>>>,
}

impl ConnectionLog {
    /// Identifier of the connection this handle logs for.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Session this connection is bound to, if any.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.read_session()
    }

    /// Write a record to whichever destination it belongs in.
    ///
    /// A record carrying its own session id goes to that session, which is what
    /// lets one connection serving several sessions keep them in separate
    /// files. Otherwise the record inherits the connection's binding, and falls
    /// back to the connection file when there is none.
    pub fn log(&self, record: LogRecord) {
        // A session id lifted off the wire is untrusted input. Routing on one
        // that is not a safe path component would let a hostile agent choose
        // where this process writes, so such a record is kept but filed under
        // the connection instead.
        if let Some(session_id) = record.session_id.clone()
            && validate_id(&session_id).is_ok()
        {
            self.sink.enqueue(Destination::Session(session_id), record);
            return;
        }

        match self.read_session() {
            Some(session_id) if record.session_id.is_none() => {
                let record = record.with_session(session_id.clone());
                self.sink.enqueue(Destination::Session(session_id), record);
            }
            _ => self
                .sink
                .enqueue(Destination::Connection(self.connection_id.clone()), record),
        }
    }

    /// Write a record to this connection's fallback file, without inheriting
    /// its session binding.
    pub fn log_fallback(&self, record: LogRecord) {
        self.sink
            .enqueue(Destination::Connection(self.connection_id.clone()), record);
    }

    /// Route this connection's subsequent records to `session_id`.
    ///
    /// Leaves a [`KIND_SESSION_BOUND`] record in the connection file recording
    /// the mapping, so records written before the session existed can still be
    /// found from the session's own log.
    ///
    /// # Errors
    ///
    /// Returns [`LogSinkError::InvalidId`] if `session_id` is not a safe path
    /// component. The binding is not applied in that case.
    pub fn bind_session(&self, session_id: &str) -> Result<(), LogSinkError> {
        validate_id(session_id)?;

        self.sink.enqueue(
            Destination::Connection(self.connection_id.clone()),
            LogRecord::text(Direction::Internal, KIND_SESSION_BOUND, session_id)
                .with_session(session_id),
        );

        let mut guard = self
            .session_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(session_id.to_string());

        Ok(())
    }

    fn read_session(&self) -> Option<String> {
        self.session_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Reject identifiers that are not safe single path components.
///
/// Applied at handle creation and at bind time rather than in the writer, so a
/// bad identifier surfaces as an error to the caller instead of vanishing into
/// a background thread.
fn validate_id(id: &str) -> Result<(), LogSinkError> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));

    if valid {
        Ok(())
    } else {
        Err(LogSinkError::InvalidId(id.to_string()))
    }
}

#[cfg(test)]
mod tests;
