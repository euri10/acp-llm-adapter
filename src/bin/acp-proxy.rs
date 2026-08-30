#![forbid(unsafe_code)]
#![deny(
    warnings,
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

//! ACP debugging proxy.
//!
//! Wraps any ACP agent that speaks the protocol over stdio and records both
//! directions of its traffic. Uses none of the adapter's LLM machinery, which
//! is why it ships as its own binary.
//!
//! ```text
//! acp-proxy -- claude-agent-acp --some-flag
//! ```
//!
//! The `--` separator is required. Without it the agent command and its flags
//! are parsed as proxy options and the proxy exits with `unexpected argument`.
//! Programmatic callers must include a literal `"--"` argv element before the
//! agent command.

use std::ffi::OsString;
use std::path::PathBuf;

use acp_llm_adapter::paths::default_proxy_log_root;
use acp_llm_adapter::proxy::{DEFAULT_QUEUE_CAPACITY, ProxyConfig, run};
use clap::Parser;

/// Record and forward the traffic of an ACP agent.
#[derive(Debug, Parser)]
#[command(name = "acp-proxy", version, about, long_about = None)]
struct Cli {
    /// Directory logs are written under.
    ///
    /// Defaults to the proxy subdirectory of this application's state
    /// directory, deliberately outside the adapter's own session store.
    #[arg(long, value_name = "PATH")]
    log_root: Option<PathBuf>,

    /// Records that may be queued before further records are dropped.
    ///
    /// Logging never blocks the forwarded streams, so a saturated queue costs
    /// log lines rather than stalling the wrapped agent.
    #[arg(long, default_value_t = DEFAULT_QUEUE_CAPACITY, value_name = "N")]
    queue_capacity: usize,

    /// The agent to run, and its arguments.
    ///
    /// Must follow a literal `--`: `acp-proxy -- <COMMAND>...`. The separator
    /// is what stops the agent name and its flags from being read as proxy
    /// options.
    #[arg(required = true, last = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<OsString>,
}

// This deliberately never returns: see `exit` below for why.
#[tokio::main]
async fn main() -> ! {
    let cli = Cli::parse();

    let Some((program, args)) = cli.command.split_first() else {
        eprintln!("acp-proxy: no agent command given");
        exit(2);
    };

    let Some(log_root) = cli.log_root.or_else(default_proxy_log_root) else {
        eprintln!("acp-proxy: neither XDG_STATE_HOME nor HOME is set; pass --log-root");
        exit(2);
    };

    let mut config = ProxyConfig::new(program.clone(), args.to_vec(), log_root);
    config.queue_capacity = cli.queue_capacity;

    match run(config).await {
        Ok(code) => exit(narrow_exit_code(code)),
        Err(error) => {
            eprintln!("acp-proxy: {error}");
            exit(1)
        }
    }
}

/// Narrow an agent's exit code onto the range a process can actually report.
fn narrow_exit_code(code: i32) -> i32 {
    match u8::try_from(code) {
        Ok(code) => i32::from(code),
        // A code outside 0..=255 cannot be reported faithfully; anything
        // non-zero still has to read as failure.
        Err(_) => 1,
    }
}

/// Terminate the process immediately, without waiting on the async runtime.
///
/// Returning an `ExitCode` from `main` instead would drop the tokio
/// `Runtime`, which blocks until every outstanding blocking-pool task
/// finishes. The stdin-forwarding task can be stuck in a blocking read on
/// this process's own stdin (e.g. a client holding a pipe open) with no way
/// to cancel it, so that drop would hang forever even after the wrapped
/// agent has exited. `std::process::exit` skips destructors entirely and
/// ends the process outright.
// The workspace denies `clippy::exit` because calling it from library code
// terminates the host process out from under the caller. This is the `main`
// of a standalone binary, the one place that call is the point.
#[allow(clippy::exit)]
fn exit(code: i32) -> ! {
    std::process::exit(code)
}
