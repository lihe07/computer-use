//! Wayland portal capture and input, with COSMIC toplevel management.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, Frame, Margin, RichText};
use layer_shell_wgpu_egui::layer_shell::LayerShellOptions;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

use anyhow::{Context, Result};
use image::DynamicImage;

use crate::desktop::DesktopBackend;
use crate::ei::{self, EiClient, Region};
use crate::portal;
use crate::windows::{Window, Windows};

#[derive(Default)]
pub struct Wayland {
    client: Option<EiClient>,
    session: Option<portal::RemoteSession>,
}

impl Wayland {
    fn input(&mut self) -> Result<&mut EiClient> {
        self.client.as_mut().context("Wayland input is disconnected")
    }
}

impl DesktopBackend for Wayland {
    fn connect(&mut self) -> Result<()> {
        if let Some(c) = self.client.as_mut() {
            c.pump(Duration::ZERO);
            if !c.disconnected {
                return Ok(());
            }
        }
        self.client = None;
        self.session = None;
        let session = portal::RemoteSession::start()?;
        let mut client = EiClient::new(session.connect_to_eis()?)?;
        client.wait_ready(ei::CAP_POINTER_ABSOLUTE | ei::CAP_BUTTON | ei::CAP_KEYBOARD, Duration::from_secs(5))?;
        self.session = Some(session);
        self.client = Some(client);
        Ok(())
    }

    fn capture(&self) -> Result<DynamicImage> {
        let path = portal::screenshot()?;
        let img = image::open(&path).with_context(|| format!("decode {}", path.display()));
        let _ = std::fs::remove_file(&path);
        Ok(DynamicImage::ImageRgb8(img?.to_rgb8()))
    }

    fn monitors(&mut self) -> Result<Vec<Region>> {
        self.input()?.regions()
    }

    fn move_abs(&mut self, x: f64, y: f64) -> Result<()> {
        self.input()?.move_abs(x, y)
    }

    fn button(&mut self, code: u32, press: bool) -> Result<()> {
        self.input()?.button(code, press)
    }

    fn key(&mut self, code: u32, press: bool) -> Result<()> {
        self.input()?.key(code, press)
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.input()?.scroll(dx, dy)
    }

    fn pump(&mut self, duration: Duration) {
        if let Some(input) = &mut self.client {
            input.pump(duration);
        }
    }

    fn windows(&mut self) -> Result<Vec<Window>> {
        let mons = self.monitors()?;
        let mut wins = Windows::connect()?.list();
        for w in &mut wins {
            for g in &mut w.geometry {
                if let Some(m) = mons.iter().find(|m| m.name == g.output) {
                    g.x += m.x as i32;
                    g.y += m.y as i32;
                }
            }
        }
        Ok(wins)
    }

    fn window_action(&mut self, query: &str, action: &str) -> Result<Window> {
        Windows::connect()?.act(query, action)
    }

    fn clipboard(&self) -> (&str, &[&str]) {
        ("wl-copy", &[])
    }
}

/// MCP hosts may launch us with a scrubbed env; recover the user session's bus and display.
pub fn prepare_env() {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(1000);
        format!("/run/user/{uid}")
    });
    // SAFETY: called before the tokio runtime spawns any threads.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus"));
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(1000);
            match crate::portal_repair::discover(uid, std::path::Path::new(&runtime)) {
                Ok(env) => {
                    for (key, value) in env {
                        if std::env::var_os(&key).is_none() {
                            std::env::set_var(key, value);
                        }
                    }
                }
                Err(e) => eprintln!("computer-use: cannot infer desktop session: {e:#}"),
            }
        }
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
pub fn hud() -> Result<()> {
    let visible = Arc::new(AtomicBool::new(true));
    let frames = Arc::new(Frames::default());
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::default();

    {
        let (visible, frames, ctx_slot) = (visible.clone(), frames.clone(), ctx_slot.clone());
        std::thread::spawn(move || {
            // The update callback runs just before the buffer is committed; give the
            // compositor a moment to present it before acknowledging.
            frames.wait_past(0, Duration::from_secs(3));
            let _ = crate::hud::serve(
                || std::thread::sleep(Duration::from_millis(50)),
                |v| {
                    let n = frames.get();
                    visible.store(v, Ordering::SeqCst);
                    if let Some(ctx) = ctx_slot.lock().unwrap().as_ref() {
                        ctx.request_repaint();
                    }
                    frames.wait_past(n, Duration::from_millis(500));
                    Ok(())
                },
            );
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
