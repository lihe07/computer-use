//! "Agent is in control" HUD: a click-through layer-shell overlay.
//!
//! The layer-shell event loop never returns, so the HUD runs as a child process
//! (`computer-use --hud`) driven over stdin/stdout: the parent writes `show` / `hide`
//! and waits for `ok`, which the child prints once the change has been presented.
//! Killing the child (or the parent exiting, which closes its stdin) removes it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use egui::{Align2, Color32, CornerRadius, Frame, Margin, RichText};
use layer_shell_wgpu_egui::layer_shell::LayerShellOptions;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

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

/// Counts drawn frames so the command thread can wait for one.
#[derive(Default)]
struct Frames {
    count: Mutex<u64>,
    cv: Condvar,
}

impl Frames {
    fn bump(&self) {
        *self.count.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    fn wait_past(&self, n: u64, timeout: Duration) {
        let g = self.count.lock().unwrap();
        let _ = self.cv.wait_timeout_while(g, timeout, |c| *c <= n);
    }

    fn get(&self) -> u64 {
        *self.count.lock().unwrap()
    }
}

/// Child-side entry point: runs the overlay until stdin closes.
pub fn run() -> Result<()> {
    let visible = Arc::new(AtomicBool::new(true));
    let frames = Arc::new(Frames::default());
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::default();

    {
        let (visible, frames, ctx_slot) = (visible.clone(), frames.clone(), ctx_slot.clone());
        std::thread::spawn(move || {
            // The update callback runs just before the buffer is committed; give the
            // compositor a moment to present it before acknowledging.
            let ack = || {
                std::thread::sleep(Duration::from_millis(50));
                println!("ok");
                let _ = std::io::stdout().flush();
            };
            frames.wait_past(0, Duration::from_secs(3));
            ack();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let v = match line.trim() {
                    "show" => true,
                    "hide" => false,
                    _ => continue,
                };
                let n = frames.get();
                visible.store(v, Ordering::SeqCst);
                if let Some(ctx) = ctx_slot.lock().unwrap().as_ref() {
                    ctx.request_repaint();
                }
                frames.wait_past(n, Duration::from_millis(500));
                ack();
            }
            std::process::exit(0); // parent went away
        });
    }

    let options = LayerShellOptions {
        layer: Some(Layer::Overlay), // above fullscreen windows
        namespace: "computer-use-hud".to_owned(),
        width: 560,
        height: 64,
        anchor: Some(Anchor::TOP),
        keyboard_interactivity: Some(KeyboardInteractivity::None),
        input_passthrough: true,
    };
    layer_shell_wgpu_egui::run_layer_simple(options, move |ctx| {
        ctx_slot.lock().unwrap().get_or_insert_with(|| ctx.clone());
        if visible.load(Ordering::SeqCst) {
            draw(ctx);
        }
        frames.bump();
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))
}

fn draw(ctx: &egui::Context) {
    egui::Area::new("hud".into()).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        Frame::new()
            .fill(Color32::from_rgb(214, 64, 20))
            .corner_radius(CornerRadius::same(22))
            .inner_margin(Margin::symmetric(18, 10))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let (dot, _) = ui.allocate_exact_size([10.0, 10.0].into(), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, Color32::WHITE);
                    ui.add_space(4.0);
                    ui.label(RichText::new("Agent is in control").strong().size(16.0).color(Color32::WHITE));
                    ui.label(
                        RichText::new("please don't touch the mouse or keyboard")
                            .size(13.0)
                            .color(Color32::from_white_alpha(220)),
                    );
                });
            });
    });
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
