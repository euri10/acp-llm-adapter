//! Process ownership for the dedicated proxy, including Linux orphan adoption.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;
use tokio::task::JoinHandle;

pub(super) struct Lifetime {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Lifetime {
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        let lifetime = Self {
            // Register before arming parent death or starting any children.
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        };
        #[cfg(not(unix))]
        let lifetime = Self {};

        #[cfg(target_os = "linux")]
        {
            use rustix::process::{self, Signal};

            let parent = process::getppid();
            process::set_child_subreaper(Some(process::getpid()))?;
            process::set_parent_process_death_signal(Some(Signal::TERM))?;
            // Death between reading the parent and arming the signal must not
            // start an unowned Agent. Earlier death is also caught by stdin EOF.
            if parent != process::getppid() {
                return Err(io::Error::other("proxy client exited during startup"));
            }
            adopted_children()?;
        }
        Ok(lifetime)
    }

    pub(super) async fn wait(
        &mut self,
        child: &mut Child,
        upstream: &mut JoinHandle<io::Result<()>>,
    ) -> io::Result<ExitStatus> {
        tokio::select! {
            status = child.wait() => return status,
            () = self.shutdown() => {},
            result = upstream => {
                result.map_err(io::Error::other)??;
                // Preserve graceful EOF and the Agent's exit status, but do
                // not trust an Agent that keeps running after its client left.
                if let Ok(status) = tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                    return status;
                }
            }
        }
        child.kill().await?;
        child.wait().await
    }

    async fn shutdown(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    }

    pub(super) async fn reclaim(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use rustix::io::Errno;
            use rustix::process::{self, Signal, WaitOptions};

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                // The directly spawned Child has already been waited on.
                // Tokio owns no handles for these adopted children; only this
                // loop reaps them, preventing PID reuse between lookup/kill.
                match process::wait(WaitOptions::NOHANG) {
                    Err(Errno::CHILD) => return Ok(()),
                    Err(Errno::INTR) | Ok(Some(_)) => continue,
                    Err(error) => return Err(error.into()),
                    Ok(None) => {}
                }
                for pid in adopted_children()? {
                    process::kill_process(pid, Signal::KILL)?;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Agent descendants did not exit",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        #[cfg(not(target_os = "linux"))]
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn adopted_children() -> io::Result<Vec<rustix::process::Pid>> {
    let mut children = Vec::new();
    // Adoption can attach children to any surviving thread in this process.
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // A runtime thread can exit during enumeration. Its children are
            // reparented within this process and found on the next pass.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for value in contents.split_whitespace() {
            let raw = value.parse().map_err(io::Error::other)?;
            let pid = rustix::process::Pid::from_raw(raw)
                .ok_or_else(|| io::Error::other("invalid adopted child PID"))?;
            children.push(pid);
        }
    }
    Ok(children)
}
