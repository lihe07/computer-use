//! "Agent is in control" HUD: a click-through layer-shell overlay.
//!
//! The layer-shell event loop never returns, so the HUD runs as a child process
//! (`computer-use --hud`) driven over stdin/stdout: the parent writes `show` / `hide`
//! and waits for `ok`, which the child prints once the change has been presented.
//! Killing the child (or the parent exiting, which closes its stdin) removes it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use egui::{Align2, Color32, CornerRadius, Frame, Margin, RichText};
use layer_shell_wgpu_egui::layer_shell::LayerShellOptions;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

pub const ARG: &str = "--hud";

/// Parent-side handle to the HUD child process.
pub struct Hud {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Hud {
    /// Spawns the HUD and waits until it is on screen.
    pub fn spawn() -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg(ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped()) // never inherit: our stdout is the MCP stream
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn HUD")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut hud = Self { child, stdin, stdout };
        hud.wait_ok().context("HUD failed to start")?;
        Ok(hud)
    }

    /// Show or hide the HUD; returns once the compositor has the new frame.
    pub fn set(&mut self, visible: bool) -> Result<()> {
        writeln!(self.stdin, "{}", if visible { "show" } else { "hide" })?;
        self.stdin.flush()?;
        self.wait_ok()
    }

    fn wait_ok(&mut self) -> Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("HUD process exited");
            }
            if line.trim() == "ok" {
                return Ok(());
            }
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
