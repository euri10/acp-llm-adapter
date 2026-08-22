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

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

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
    #[arg(required = true, last = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<OsString>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some((program, args)) = cli.command.split_first() else {
        eprintln!("acp-proxy: no agent command given");
        return ExitCode::from(2);
    };

    let Some(log_root) = cli.log_root.or_else(default_proxy_log_root) else {
        eprintln!("acp-proxy: neither XDG_STATE_HOME nor HOME is set; pass --log-root");
        return ExitCode::from(2);
    };

    let mut config = ProxyConfig::new(program.clone(), args.to_vec(), log_root);
    config.queue_capacity = cli.queue_capacity;

    match run(config).await {
        Ok(code) => exit_code(code),
        Err(error) => {
            eprintln!("acp-proxy: {error}");
            ExitCode::from(1)
        }
    }
}

/// Narrow an agent's exit code onto the range a process can actually report.
fn exit_code(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        // A code outside 0..=255 cannot be reported faithfully; anything
        // non-zero still has to read as failure.
        Err(_) => ExitCode::from(1),
    }
}
