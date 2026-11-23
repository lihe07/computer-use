//! Process protocol for the click-through "Agent is in control" HUD.
//!
//! The HUD runs as `computer-use --hud` and takes commands over stdin/stdout.
//! The parent writes `show` / `hide`
//! and waits for `ok`, which the child prints once the change has been presented.
//! Killing the child (or the parent exiting, which closes its stdin) removes it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const ARG: &str = "--hud";

/// How long the child gets to put its first frame on screen.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a show/hide acknowledgement may take.
const SET_TIMEOUT: Duration = Duration::from_secs(5);

/// Parent-side handle to the HUD child process. Dropping it kills the child.
pub struct Hud {
    child: Child,
    stdin: ChildStdin,
    /// One message per "ok" line; disconnects when the child's stdout closes.
    acks: Receiver<()>,
}

impl Hud {
    /// Spawns the HUD and waits (bounded) until it is on screen.
    pub fn spawn() -> Result<Self> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg(ARG);
        Self::spawn_command(cmd, SPAWN_TIMEOUT)
    }

    fn spawn_command(mut cmd: Command, timeout: Duration) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped()) // never inherit: our stdout is the MCP stream
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn HUD")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, acks) = mpsc::channel();
        // Reads blocking pipe I/O off the command thread so waits can time out.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) if l.trim() == "ok" => {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        // From here, any early return drops `hud`, which kills the child.
        let mut hud = Self { child, stdin, acks };
        hud.wait_ok(timeout).context("HUD failed to start")?;
        Ok(hud)
    }

    /// Show or hide the HUD; returns once the compositor has the new frame.
    pub fn set(&mut self, visible: bool) -> Result<()> {
        writeln!(self.stdin, "{}", if visible { "show" } else { "hide" })?;
        self.stdin.flush()?;
        self.wait_ok(SET_TIMEOUT)
    }

    fn wait_ok(&mut self, timeout: Duration) -> Result<()> {
        match self.acks.recv_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => bail!("HUD did not respond within {}s", timeout.as_secs_f32()),
            Err(RecvTimeoutError::Disconnected) => bail!("HUD process exited"),
        }
    }
}

impl Drop for Hud {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Acknowledge initial visibility and each completed show/hide request.
pub fn serve(before_ack: impl Fn(), mut set: impl FnMut(bool) -> Result<()>) -> Result<()> {
    let ack = || -> Result<()> {
        before_ack();
        println!("ok");
        Ok(std::io::stdout().flush()?)
    };
    ack()?;
    for line in std::io::stdin().lock().lines() {
        match line?.trim() {
            "show" => set(true)?,
            "hide" => set(false)?,
            _ => continue,
        }
        ack()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    fn gone(pid: u32) -> bool {
        // Killed and reaped, so /proc/<pid> disappears.
        std::fs::metadata(format!("/proc/{pid}")).is_err()
    }

    #[test]
    fn unresponsive_hud_times_out_and_is_killed() {
        let started = Instant::now();
        // Never acknowledges, like a child stuck in compositor setup.
        let r = Hud::spawn_command(sh("exec sleep 60"), Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        let err = format!("{:#}", r.err().expect("must fail"));
        assert!(err.contains("did not respond"), "{err}");
    }

    #[test]
    fn timed_out_hud_child_is_reaped() {
        let mut cmd = sh("exec sleep 60");
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        // Same teardown path as a failed spawn: Hud owns the child and is dropped.
        let (stdin, mut child) = {
            let mut c = child;
            (c.stdin.take().unwrap(), c)
        };
        let (_tx, acks) = mpsc::channel();
        child.stdout.take();
        drop(Hud { child, stdin, acks });
        assert!(gone(pid));
    }

    #[test]
    fn exiting_hud_is_reported() {
        let r = Hud::spawn_command(sh("exit 3"), Duration::from_secs(5));
        assert!(format!("{:#}", r.err().unwrap()).contains("exited"));
    }

    #[test]
    fn acknowledging_hud_works() {
        let mut hud = Hud::spawn_command(sh("echo ok; while read l; do echo ok; done"), Duration::from_secs(5)).unwrap();
        hud.set(false).unwrap();
        hud.set(true).unwrap();
        let pid = hud.child.id();
        drop(hud);
        assert!(gone(pid));
    }
}
