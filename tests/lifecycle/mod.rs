//! Linux proxy lifecycle regressions (louiselm-xq6c / daa-eyj2).
//!
//! The live Codex tree in Session codex/01a07af7-74e1-7cb3-a89d-6549dad3ce23
//! contained tool descendants with independent process groups and sessions.
//! This fixture models that topology, not any assumed ACP frame sequence.

use std::io;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{TempRoot, fixture_binary, proxy_binary};

struct Tree {
    root: TempRoot,
    client: Child,
}

impl Tree {
    fn start() -> io::Result<Self> {
        Self::start_with_inherited_stdin(false)
    }

    fn start_with_inherited_stdin(keep_stdin: bool) -> io::Result<Self> {
        let root = TempRoot::new("tree");
        std::fs::create_dir_all(root.path())?;
        let mut command = Command::new(fixture_binary());
        if keep_stdin {
            command.env("ACP_PROXY_TREE_HOLD_STDIN", "1");
        }
        let client = command
            .arg(proxy_binary())
            .env("ACP_PROXY_TREE_ROLE", "client")
            .env("ACP_PROXY_TREE_PIDS", root.path().join("pids"))
            .env("ACP_PROXY_TREE_LOGS", root.path().join("logs"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let tree = Self { root, client };
        let count = if keep_stdin { 6 } else { 5 };
        assert!(
            wait_until(|| tree.pids().len() == count),
            "tree did not start"
        );
        Ok(tree)
    }

    fn pids(&self) -> Vec<(String, u32)> {
        std::fs::read_to_string(self.root.path().join("pids"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (role, pid) = line.split_once(' ')?;
                Some((role.to_string(), pid.parse().ok()?))
            })
            .collect()
    }

    fn pid(&self, role: &str) -> u32 {
        match self.pids().into_iter().find(|(name, _)| name == role) {
            Some((_, pid)) => pid,
            None => unreachable!("missing {role} pid"),
        }
    }

    fn assert_descendants_gone(&self) {
        assert!(
            wait_until(|| self
                .pids()
                .iter()
                .filter(|(role, _)| role != "client" && role != "holder")
                .all(|(_, pid)| !alive(*pid))),
            "proxy descendants survived: {:?}",
            self.pids()
                .into_iter()
                .filter(|(_, pid)| alive(*pid))
                .collect::<Vec<_>>()
        );
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // Failure cleanup targets only PIDs recorded by this fixture; never
        // discover or kill another editor's Sessions by name or process group.
        for (_, pid) in self.pids().into_iter().rev() {
            if alive(pid) {
                let _ = signal(pid, "KILL");
            }
        }
        // The controller may already have exited or been reaped by the test.
        let _ = self.client.kill();
        let _ = self.client.wait();
    }
}

fn signal(pid: u32, name: &str) -> io::Result<()> {
    let status = Command::new("kill")
        .args(["-s", name, "--", &pid.to_string()])
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("fixture signal failed"))
    }
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn client_death_reaps_detached_descendants_without_touching_another_session() -> io::Result<()> {
    let mut killed = Tree::start()?;
    let neighbor = Tree::start()?;
    killed.client.kill()?;
    killed.client.wait()?;
    killed.assert_descendants_gone();
    assert!(neighbor.pids().iter().all(|(_, pid)| alive(*pid)));
    Ok(())
}

#[test]
fn proxy_termination_reaps_detached_descendants() -> io::Result<()> {
    let tree = Tree::start()?;
    signal(tree.pid("proxy"), "TERM")?;
    tree.assert_descendants_gone();
    Ok(())
}

#[test]
fn parent_death_does_not_depend_on_client_stdin_reaching_eof() -> io::Result<()> {
    let mut tree = Tree::start_with_inherited_stdin(true)?;
    tree.client.kill()?;
    tree.client.wait()?;
    tree.assert_descendants_gone();
    assert!(
        alive(tree.pid("holder")),
        "the unrelated pipe holder must survive"
    );
    Ok(())
}

#[test]
fn wrapped_agent_exit_reaps_descendants_that_hold_its_output_open() -> io::Result<()> {
    let tree = Tree::start()?;
    signal(tree.pid("agent"), "KILL")?;
    tree.assert_descendants_gone();
    Ok(())
}

#[test]
fn client_eof_bounds_an_uncooperative_agent_lifetime() -> io::Result<()> {
    let root = TempRoot::new("tree-eof");
    std::fs::create_dir_all(root.path())?;
    let client = Command::new(proxy_binary())
        .arg("--log-root")
        .arg(root.path().join("logs"))
        .arg("--")
        .arg(fixture_binary())
        .env("ACP_PROXY_TREE_ROLE", "agent")
        .env("ACP_PROXY_TREE_PIDS", root.path().join("pids"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut tree = Tree { root, client };
    assert!(wait_until(|| tree.pids().len() == 3));
    drop(tree.client.stdin.take());
    let mut status = None;
    assert!(wait_until(|| {
        status = tree.client.try_wait().ok().flatten();
        status.is_some()
    }));
    assert_eq!(status.and_then(|value| value.code()), Some(137));
    tree.assert_descendants_gone();
    Ok(())
}
