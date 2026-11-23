//! Backend selection and shared screenshot and input processing for the worker.

use std::io::Cursor;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use serde::{Deserialize, Serialize};

use crate::ei::Region;
use crate::keys;
use crate::{wayland, windows::Window, x11};

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Maps pixels of the last returned image back to logical desktop coordinates.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Shot {
    pub origin_x: f64,
    pub origin_y: f64,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

impl Shot {
    pub fn to_desktop(&self, x: f64, y: f64) -> (f64, f64) {
        (self.origin_x + x / self.scale, self.origin_y + y / self.scale)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PasteKey {
    Auto,
    CtrlV,
    CtrlShiftV,
}

pub struct Backend {
    pub prepare_env: fn(),
    pub hud: fn() -> Result<()>,
    pub open: fn() -> Result<Box<dyn DesktopBackend>>,
}

impl Backend {
    pub fn select() -> Self {
        let present = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let x11 =
            std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("x11") || (present("DISPLAY") && !present("WAYLAND_DISPLAY"));
        if x11 {
            Self { prepare_env: || {}, hud: x11::hud, open: || Ok(Box::new(x11::X11::open()?)) }
        } else {
            Self { prepare_env: wayland::prepare_env, hud: wayland::hud, open: || Ok(Box::new(wayland::Wayland::default())) }
        }
    }
}

pub trait DesktopBackend {
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    fn capture(&self) -> Result<DynamicImage>;
    fn monitors(&mut self) -> Result<Vec<Region>>;
    fn move_abs(&mut self, x: f64, y: f64) -> Result<()>;
    fn button(&mut self, code: u32, press: bool) -> Result<()>;
    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()>;
    fn key(&mut self, code: u32, press: bool) -> Result<()>;
    fn pump(&mut self, duration: Duration) {
        sleep(duration);
    }
    fn windows(&mut self) -> Result<Vec<Window>>;
    fn window_action(&mut self, query: &str, action: &str) -> Result<Window>;
    fn clipboard(&self) -> (&str, &[&str]);
}

pub struct Desktop(pub Box<dyn DesktopBackend>);

impl Desktop {
    pub fn connect(&mut self) -> Result<()> {
        self.0.connect()
    }

    /// Logical monitor rects, left to right.
    pub fn monitors(&mut self) -> Result<Vec<Region>> {
        let mut monitors = self.0.monitors()?;
        monitors.sort_by_key(|m| (m.x, m.y));
        Ok(monitors)
    }

    // -- screenshots ----------------------------------------------------------------------

    /// Returns PNG bytes. `region` = (x, y, w, h) in logical desktop coords; zoomed up to 4x.
    pub fn screenshot(
        &mut self,
        monitor: Option<&str>,
        region: Option<(i64, i64, i64, i64)>,
        max_w: u32,
        max_h: u32,
    ) -> Result<(Vec<u8>, Shot)> {
        let mut img = self.0.capture()?;

        let region = match monitor {
            Some(m) => {
                let mons = self.monitors()?;
                let r = mons
                    .iter()
                    .find(|r| r.name == m)
                    .or_else(|| m.parse::<usize>().ok().and_then(|i| mons.get(i)))
                    .ok_or_else(|| {
                        anyhow!("unknown monitor {m:?}; have {:?}", mons.iter().map(|r| &r.name).collect::<Vec<_>>())
                    })?;
                Some((r.x as i64, r.y as i64, r.w as i64, r.h as i64))
            }
            None => region,
        };
        let (mut ox, mut oy) = (0.0, 0.0);
        let zoomable = region.is_some();
        if let Some((x, y, w, h)) = region {
            let (x, y) = (x.max(0) as u32, y.max(0) as u32);
            let w = (w.max(1) as u32).min(img.width().saturating_sub(x));
            let h = (h.max(1) as u32).min(img.height().saturating_sub(y));
            if w == 0 || h == 0 {
                bail!("region is outside the desktop");
            }
            img = img.crop_imm(x, y, w, h);
            (ox, oy) = (x as f64, y as f64);
        }
        let cap = if zoomable { 4.0 } else { 1.0 };
        let scale = (max_w as f64 / img.width() as f64).min(max_h as f64 / img.height() as f64).min(cap);
        if (scale - 1.0).abs() > 1e-6 {
            let (w, h) = ((img.width() as f64 * scale).round() as u32, (img.height() as f64 * scale).round() as u32);
            img = img.resize_exact(w.max(1), h.max(1), FilterType::Lanczos3);
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png)?;
        let shot = Shot { origin_x: ox, origin_y: oy, scale, width: img.width(), height: img.height() };
        Ok((buf.into_inner(), shot))
    }

    // -- input ----------------------------------------------------------------------------

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.0.move_abs(x, y)
    }

    pub fn click(&mut self, x: f64, y: f64, button: &str, count: u32, modifiers: &str) -> Result<()> {
        let code = keys::button(button)?;
        let mods = if modifiers.trim().is_empty() { vec![] } else { keys::parse_combo(modifiers)? };
        let c = &mut self.0;
        c.move_abs(x, y)?;
        sleep(ms(30));
        for &m in &mods {
            c.key(m, true)?;
        }
        for _ in 0..count.max(1) {
            c.button(code, true)?;
            sleep(ms(20));
            c.button(code, false)?;
            sleep(ms(60));
        }
        for &m in mods.iter().rev() {
            c.key(m, false)?;
        }
        c.pump(ms(20));
        Ok(())
    }

    pub fn drag(&mut self, from: (f64, f64), to: (f64, f64), button: &str) -> Result<()> {
        let code = keys::button(button)?;
        let c = &mut self.0;
        c.move_abs(from.0, from.1)?;
        sleep(ms(30));
        c.button(code, true)?;
        let steps = 20;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            c.move_abs(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)?;
            sleep(ms(15));
        }
        sleep(ms(50));
        c.button(code, false)?;
        c.pump(ms(20));
        Ok(())
    }

    pub fn scroll(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let c = &mut self.0;
        c.move_abs(x, y)?;
        sleep(ms(20));
        c.scroll(dx, dy)?;
        c.pump(ms(20));
        Ok(())
    }

    pub fn key(&mut self, combo: &str, repeat: u32) -> Result<()> {
        let codes = keys::parse_combo(combo)?;
        let c = &mut self.0;
        for _ in 0..repeat.max(1) {
            for &k in &codes {
                c.key(k, true)?;
            }
            sleep(ms(20));
            for &k in codes.iter().rev() {
                c.key(k, false)?;
            }
            sleep(ms(20));
        }
        c.pump(ms(20));
        Ok(())
    }

    /// Types ASCII as US-layout keystrokes; anything else is pasted via the clipboard.
    pub fn type_text(&mut self, text: &str, paste: PasteKey) -> Result<String> {
        let keys: Option<Vec<(u32, bool)>> = text.chars().map(keys::char_key).collect();
        let Some(keys) = keys else { return self.paste(text, paste) };
        let c = &mut self.0;
        for (code, shift) in keys {
            if shift {
                c.key(keys::SHIFT, true)?;
            }
            c.key(code, true)?;
            c.key(code, false)?;
            if shift {
                c.key(keys::SHIFT, false)?;
            }
            sleep(ms(8));
        }
        c.pump(ms(20));
        Ok("typed".into())
    }

    fn paste(&mut self, text: &str, paste: PasteKey) -> Result<String> {
        let (program, args) = self.0.clipboard();
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .with_context(|| format!("non-ASCII text needs {program}"))?;
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("{program} failed: {status}");
        }
        sleep(ms(80));
        let shift = match paste {
            PasteKey::CtrlV => false,
            PasteKey::CtrlShiftV => true,
            PasteKey::Auto => self.0.windows().is_ok_and(|wins| wins.iter().any(is_focused_terminal)),
        };
        let combo = if shift { "ctrl+shift+v" } else { "ctrl+v" };
        self.key(combo, 1)?;
        Ok(format!("pasted via clipboard ({combo})"))
    }

    // -- windows --------------------------------------------------------------------------

    /// Windows with geometry translated to logical desktop coordinates.
    pub fn windows(&mut self) -> Result<Vec<Window>> {
        self.0.windows()
    }

    pub fn window_action(&mut self, query: &str, action: &str) -> Result<Window> {
        let w = self.0.window_action(query, action)?;
        // Let workspace-switch / maximize animations settle before the next screenshot.
        sleep(ms(400));
        Ok(w)
    }
}

const TERMINALS: &[&str] =
    &["foot", "wezterm", "kitty", "alacritty", "terminal", "konsole", "ghostty", "xterm", "term", "tilix", "rio"];

fn is_focused_terminal(w: &Window) -> bool {
    let id = w.app_id.to_lowercase();
    w.states.contains(&"focused") && TERMINALS.iter().any(|t| id.contains(t))
}
