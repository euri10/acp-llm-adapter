//! ACP debugging proxy.
//!
//! Wraps an external ACP agent — `claude-agent-acp`, `codex-acp`, or anything
//! else that speaks the protocol over stdio — and records both directions of
//! its traffic through the shared [`crate::logsink`].
//!
//! # Transparency
//!
//! Bytes are forwarded exactly as received. The logging path splits a *copy*
//! of the stream into lines; nothing is ever reconstructed from parsed JSON and
//! written back out. Reserialising would change key order, number formatting,
//! and drop unknown fields — which would make attaching the debugger alter the
//! behaviour being debugged.
//!
//! # What this replaces
//!
//! The shell wrapper this supersedes teed the agent's stdout and captured its
//! stderr, so client-to-agent traffic was never recorded at all, and it piped
//! into `tee` without `pipefail`, so a crashed agent reported a clean exit.
//! Both are fixed here.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

use crate::logsink::{ConnectionLog, Direction, KIND_FRAME, LogRecord, LogSink, LogSinkError};

mod lifecycle;

/// Record kind for a line the wrapped agent wrote to stderr.
pub const KIND_STDERR: &str = "stderr";
/// Record kind emitted once the wrapped agent has exited.
pub const KIND_EXIT: &str = "exit";
/// Record kind describing how the proxy was invoked.
pub const KIND_LAUNCH: &str = "launch";

/// Default number of records that may be queued before records are dropped.
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;

/// Size of each read from a forwarded stream.
const READ_CHUNK: usize = 8192;

/// Cap on a single unterminated line before it is logged as-is.
///
/// A producer that never emits a newline would otherwise grow this buffer
/// without bound.
const MAX_PENDING_LINE: usize = 8 * 1024 * 1024;

/// Error returned while running the proxy.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// The wrapped agent could not be started.
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        /// The program the proxy tried to run.
        program: String,
        /// The underlying spawn failure.
        source: std::io::Error,
    },
    /// The wrapped agent did not expose one of its standard streams.
    #[error("wrapped agent is missing its {0} stream")]
    MissingStream(&'static str),
    /// Proxy I/O failed.
    #[error("proxy I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The log sink could not be set up.
    #[error("log sink error: {0}")]
    LogSink(#[from] LogSinkError),
}

/// How to run the proxy.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Program to run as the wrapped ACP agent.
    pub program: OsString,
    /// Arguments passed to the wrapped agent.
    pub args: Vec<OsString>,
    /// Directory logs are written under.
    pub log_root: PathBuf,
    /// Records that may be queued before further records are dropped.
    pub queue_capacity: usize,
    /// Identifier for this connection's log file.
    pub connection_id: String,
}

impl ProxyConfig {
    /// Build a configuration with a fresh connection id and the default queue.
    pub fn new(
        program: impl Into<OsString>,
        args: Vec<OsString>,
        log_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            log_root: log_root.into(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            connection_id: Uuid::new_v4().to_string(),
        }
    }
}

/// Run the wrapped agent to completion, returning the exit code to report.
///
/// The proxy exits with the wrapped agent's own status: its exit code, or
/// `128 + signal` when it was killed by a signal.
///
/// Run only in a dedicated proxy process that owns no unrelated children. On
/// Linux this installs parent-death notification and makes the process a child
/// subreaper so detached Agent descendants can be terminated and reaped.
///
/// # Errors
///
/// Returns [`ProxyError::Spawn`] if the agent cannot be started,
/// [`ProxyError::MissingStream`] if it exposes no stdio, and
/// [`ProxyError::LogSink`] if logging cannot be set up. [`ProxyError::Io`]
/// reports lifecycle setup, forwarding, or cleanup failures.
pub async fn run(config: ProxyConfig) -> Result<i32, ProxyError> {
    let mut lifetime = lifecycle::Lifetime::new()?;
    let sink = LogSink::spawn(&config.log_root, config.queue_capacity)?;
    let connection = sink.connection(config.connection_id.clone())?;

    connection.log(LogRecord::new(
        Direction::Internal,
        KIND_LAUNCH,
        serde_json::json!({
            "program": config.program.to_string_lossy(),
            "args": config
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        }),
    ));

    let result = pump_child(&config, &connection, &mut lifetime).await;
    sink.flush();
    result
}

/// Spawn the agent, forward its streams, and wait for it to exit.
async fn pump_child(
    config: &ProxyConfig,
    connection: &ConnectionLog,
    lifetime: &mut lifecycle::Lifetime,
) -> Result<i32, ProxyError> {
    let mut child = Command::new(&config.program)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ProxyError::Spawn {
            program: config.program.to_string_lossy().into_owned(),
            source,
        })?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or(ProxyError::MissingStream("stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or(ProxyError::MissingStream("stdout"))?;
    let child_stderr = child
        .stderr
        .take()
        .ok_or(ProxyError::MissingStream("stderr"))?;

    // Client to agent. This one may never finish on its own — a client can hold
    // stdin open for the life of the session — so it is not awaited below.
    let mut upstream = tokio::spawn({
        pump(
            tokio::io::stdin(),
            child_stdin,
            connection.clone(),
            |line| record_for(Direction::ClientToAgent, KIND_FRAME, line),
        )
    });

    // Descendants can inherit these pipes; reclaim them before draining EOF.
    let mut downstream = tokio::spawn({
        pump(
            child_stdout,
            tokio::io::stdout(),
            connection.clone(),
            |line| record_for(Direction::AgentToClient, KIND_FRAME, line),
        )
    });
    let mut diagnostics = tokio::spawn(pump(
        child_stderr,
        tokio::io::stderr(),
        connection.clone(),
        |line| record_for(Direction::Internal, KIND_STDERR, line),
    ));

    let status = lifetime.wait(&mut child, &mut upstream).await;
    // Even a forwarding or wait error must pass through descendant cleanup.
    let stopped = if status.is_err() {
        child.kill().await
    } else {
        Ok(())
    };
    let reclaimed = lifetime.reclaim().await;

    // A disconnected client may no longer drain stdout. Do not let that keep
    // the proxy alive after its entire Agent tree has been reclaimed.
    let drained = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        (&mut downstream).await.map_err(std::io::Error::other)??;
        (&mut diagnostics).await.map_err(std::io::Error::other)??;
        Ok::<_, std::io::Error>(())
    })
    .await;
    upstream.abort();
    downstream.abort();
    diagnostics.abort();

    stopped?;
    reclaimed?;
    let code = exit_code(status?);
    connection.log(LogRecord::new(
        Direction::Internal,
        KIND_EXIT,
        serde_json::json!({ "code": code }),
    ));

    drained.map_err(std::io::Error::other)??;
    Ok(code)
}

/// Forward every byte from `reader` to `writer`, logging a copy as lines.
///
/// `make_record` turns one complete line into the record to log. Building the
/// record is the caller's business because only the frame directions need the
/// session sniffing, and only they pay for it.
async fn pump<R, W, F>(
    mut reader: R,
    mut writer: W,
    log: ConnectionLog,
    make_record: F,
) -> Result<(), std::io::Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(&[u8]) -> LogRecord,
{
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut splitter = LineSplitter::default();

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).unwrap_or_default();

        // Attribute requests before forwarding: the other peer may reply as
        // soon as it reads them. The original bytes still pass through verbatim.
        for line in splitter.push(chunk) {
            log.log(make_record(&line));
        }
        writer.write_all(chunk).await?;
        writer.flush().await?;
    }

    if let Some(tail) = splitter.finish() {
        log.log(make_record(&tail));
    }

    // Closing the far end propagates EOF — this is what lets a client closing
    // its stdin shut the wrapped agent down cleanly.
    let _ = writer.shutdown().await;
    Ok(())
}

/// Build a record for one forwarded line.
fn record_for(direction: Direction, kind: &'static str, line: &[u8]) -> LogRecord {
    let text = String::from_utf8_lossy(line);
    if kind == KIND_FRAME {
        LogRecord::frame(direction, &text)
    } else {
        LogRecord::text(direction, kind, text)
    }
}

/// Map a child's exit status onto the code this process should report.
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }

    1
}

/// Splits a byte stream into newline-terminated lines across read boundaries.
///
/// ACP over stdio is NDJSON, so a line is the unit of interest. Reads do not
/// respect message boundaries, so partial lines are held until completed.
#[derive(Debug, Default)]
struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// Absorb a chunk, returning every line it completed.
    fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut rest = chunk;

        while let Some(index) = rest.iter().position(|byte| *byte == b'\n') {
            let (head, tail) = rest.split_at(index);
            self.pending.extend_from_slice(head);
            lines.push(std::mem::take(&mut self.pending));
            rest = tail.get(1..).unwrap_or_default();
        }

        self.pending.extend_from_slice(rest);
        if self.pending.len() > MAX_PENDING_LINE {
            lines.push(std::mem::take(&mut self.pending));
        }

        lines
    }

    /// Return any unterminated trailing bytes.
    fn finish(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }
}

/// Resolve a sink handle for callers that want to log outside [`run`].
///
/// Exposed so a caller embedding the proxy can share one sink across several
/// wrapped agents rather than spawning a writer per agent.
///
/// # Errors
///
/// Returns [`LogSinkError::Io`] if the writer thread cannot be spawned.
pub fn sink_for(root: impl Into<PathBuf>, capacity: usize) -> Result<Arc<LogSink>, LogSinkError> {
    LogSink::spawn(root, capacity)
}

#[cfg(test)]
mod tests;
