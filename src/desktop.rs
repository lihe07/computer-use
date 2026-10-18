//! Stateful desktop controller: portal session + EI connection, screenshot mapping.

use std::io::Cursor;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::ei::{self, EiClient, Region};
use crate::keys;
use crate::portal;
use crate::windows::{Window, Windows};

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Maps pixels of the last returned image back to logical desktop coordinates.
#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PasteKey {
    Auto,
    CtrlV,
    CtrlShiftV,
}

pub struct Desktop {
    session: Option<portal::RemoteSession>,
    client: Option<EiClient>,
    pub last_shot: Option<Shot>,
}

impl Desktop {
    pub fn new() -> Self {
        Self { session: None, client: None, last_shot: None }
    }

    fn client(&mut self) -> Result<&mut EiClient> {
        if let Some(c) = self.client.as_mut() {
            c.pump(Duration::ZERO);
            if !c.disconnected {
                return Ok(self.client.as_mut().unwrap());
            }
        }
        self.client = None;
        self.session = None;
        let session = portal::RemoteSession::start()?;
        let mut client = EiClient::new(session.connect_to_eis()?)?;
        client.wait_ready(ei::CAP_POINTER_ABSOLUTE | ei::CAP_BUTTON | ei::CAP_KEYBOARD, Duration::from_secs(5))?;
        self.session = Some(session);
        Ok(self.client.insert(client))
    }

    /// Logical monitor rects, left to right.
    pub fn monitors(&mut self) -> Result<Vec<Region>> {
        let mut r = self.client()?.regions()?;
        r.sort_by_key(|m| (m.x, m.y));
        Ok(r)
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
        let path = portal::screenshot()?;
        let img = image::open(&path).with_context(|| format!("decode {}", path.display()));
        let _ = std::fs::remove_file(&path);
        let mut img = DynamicImage::ImageRgb8(img?.to_rgb8());

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
        self.last_shot = Some(shot);
        Ok((buf.into_inner(), shot))
    }

    pub fn resolve(&self, x: f64, y: f64, desktop: bool) -> Result<(f64, f64)> {
        if desktop {
            return Ok((x, y));
        }
        let shot = self.last_shot.ok_or_else(|| anyhow!("no screenshot taken yet; take one or use space=desktop"))?;
        Ok(shot.to_desktop(x, y))
    }

    // -- input ----------------------------------------------------------------------------

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.client()?.move_abs(x, y)
    }

    pub fn click(&mut self, x: f64, y: f64, button: &str, count: u32, modifiers: &str) -> Result<()> {
        let code = keys::button(button)?;
        let mods = if modifiers.trim().is_empty() { vec![] } else { keys::parse_combo(modifiers)? };
        let c = self.client()?;
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
        let c = self.client()?;
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
        let c = self.client()?;
        c.move_abs(x, y)?;
        sleep(ms(20));
        c.scroll(dx, dy)?;
        c.pump(ms(20));
        Ok(())
    }

    pub fn key(&mut self, combo: &str, repeat: u32) -> Result<()> {
        let codes = keys::parse_combo(combo)?;
        let c = self.client()?;
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
        let c = self.client()?;
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
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .spawn()
            .context("non-ASCII text needs wl-copy (wl-clipboard)")?;
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), text.as_bytes())?;
        drop(child.stdin.take());
        child.wait()?;
        sleep(ms(80));
        let shift = match paste {
            PasteKey::CtrlV => false,
            PasteKey::CtrlShiftV => true,
            PasteKey::Auto => focused_is_terminal(),
        };
        let combo = if shift { "ctrl+shift+v" } else { "ctrl+v" };
        self.key(combo, 1)?;
        Ok(format!("pasted via clipboard ({combo})"))
    }

    // -- windows --------------------------------------------------------------------------

    /// Windows with geometry translated to logical desktop coordinates.
    pub fn windows(&mut self) -> Result<Vec<Window>> {
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

    pub fn window_action(&mut self, query: &str, action: &str) -> Result<Window> {
        let w = Windows::connect()?.act(query, action)?;
        // Let workspace-switch / maximize animations settle before the next screenshot.
        sleep(ms(400));
        Ok(w)
    }
}

const TERMINALS: &[&str] = &[
    "foot", "wezterm", "kitty", "alacritty", "terminal", "konsole", "ghostty", "xterm", "term", "tilix", "rio",
];

fn focused_is_terminal() -> bool {
    Windows::connect()
        .map(|w| {
            w.list().iter().any(|w| {
                let id = w.app_id.to_lowercase();
                w.states.contains(&"focused") && TERMINALS.iter().any(|t| id.contains(t))
            })
        })
        .unwrap_or(false)
}
