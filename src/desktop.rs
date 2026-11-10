//! Desktop engine, run inside the worker process (worker.rs): portal session, EI
//! connection, screenshots, input, the window list and the clipboard. Control, the HUD
//! and screenshot coordinates stay in the server (controller.rs).
//!
//! Reads (screenshots, monitors, windows) talk to the compositor directly (wl.rs,
//! windows.rs) and never need the portal. Only input needs the RemoteDesktop session.

use std::io::{Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use serde::{Deserialize, Serialize};

use crate::ei::{self, EiClient};
use crate::keys;
use crate::portal;
use crate::windows::{Window, Windows};
use crate::wl::{self, Monitor};

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// How long one call waits on a pending remote-desktop negotiation before returning.
pub const APPROVAL_POLL: Duration = Duration::from_secs(20);
/// Keystroke typing runs ~10ms per character; past this, a call would outlive the MCP
/// client's 120s tool-call limit and keep typing after the agent gave up on it.
pub const MAX_TYPED_CHARS: usize = 3000;
/// Bound for clipboard helper processes.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

pub const APPROVAL_PENDING: &str = "waiting for the user to allow remote control: the desktop is showing a \
    permission dialog. Ask the user to approve it (and to let it be remembered). It stays open for 5 minutes and \
    the next call picks up the approval, so do not retry in a loop. Screenshots, `windows` and `monitors` work \
    meanwhile";

/// Modifier keys released after an interrupted action (see `Desktop::connect`).
const MODIFIERS: [u32; 8] = [29, 97, 42, 54, 56, 100, 125, 126];

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

pub struct Desktop {
    session: Option<portal::RemoteSession>,
    pending: Option<portal::Pending>,
    client: Option<EiClient>,
}

/// Input on one EI connection that cannot leave a key or button held down: whatever is
/// still pressed when it is dropped (error, early return, panic) is released.
struct Input<'a> {
    c: &'a mut EiClient,
    keys: Vec<u32>,
    buttons: Vec<u32>,
}

impl Input<'_> {
    fn key(&mut self, code: u32, down: bool) -> Result<()> {
        self.c.key(code, down)?;
        if down {
            self.keys.push(code);
        } else {
            self.keys.retain(|&k| k != code);
        }
        Ok(())
    }

    fn button(&mut self, code: u32, down: bool) -> Result<()> {
        self.c.button(code, down)?;
        if down {
            self.buttons.push(code);
        } else {
            self.buttons.retain(|&b| b != code);
        }
        Ok(())
    }

    fn tap(&mut self, chord: &[u32]) -> Result<()> {
        for &k in chord {
            self.key(k, true)?;
        }
        sleep(ms(20));
        for &k in chord.iter().rev() {
            self.key(k, false)?;
        }
        Ok(())
    }
}

impl Drop for Input<'_> {
    fn drop(&mut self) {
        for &b in self.buttons.iter().rev() {
            let _ = self.c.button(b, false);
        }
        for &k in self.keys.iter().rev() {
            let _ = self.c.key(k, false);
        }
        self.c.pump(ms(20));
    }
}

impl Desktop {
    pub fn new() -> Self {
        Self { session: None, pending: None, client: None }
    }

    /// Bring up (or check) the portal session and input connection. `release` sends
    /// key-up for every modifier first: set after a worker was killed mid-action, which
    /// may have left one held on the compositor side.
    pub fn connect(&mut self, release: bool) -> Result<()> {
        let fresh = self.client.is_none();
        let c = self.client()?;
        if release || fresh {
            for k in MODIFIERS {
                let _ = c.key(k, false);
            }
            c.pump(ms(20));
        }
        Ok(())
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
        let pending = self.pending.get_or_insert_with(portal::RemoteSession::begin);
        let session = match pending.wait(APPROVAL_POLL) {
            Ok(Some(s)) => {
                self.pending = None;
                s
            }
            Ok(None) => bail!(APPROVAL_PENDING),
            Err(e) => {
                self.pending = None;
                return Err(e);
            }
        };
        let mut client = EiClient::new(session.connect_to_eis()?)?;
        client.wait_ready(ei::CAP_POINTER_ABSOLUTE | ei::CAP_BUTTON | ei::CAP_KEYBOARD, Duration::from_secs(5))?;
        // Devices that just resumed can drop the first events (lost leading characters).
        sleep(ms(150));
        client.pump(Duration::ZERO);
        self.session = Some(session);
        Ok(self.client.insert(client))
    }

    fn input(&mut self) -> Result<Input<'_>> {
        Ok(Input { c: self.client()?, keys: Vec::new(), buttons: Vec::new() })
    }

    /// Logical monitor rects, left to right.
    pub fn monitors(&mut self) -> Result<Vec<Monitor>> {
        Ok(wl::Display::connect()?.monitors())
    }

    // -- screenshots ----------------------------------------------------------------------

    /// The whole desktop at logical resolution and the logical position of its top-left
    /// pixel: direct capture, or the screenshot portal if the compositor refuses that.
    fn capture(&mut self) -> Result<(DynamicImage, (i32, i32))> {
        let direct = wl::Display::connect().and_then(|mut d| d.capture_desktop());
        match direct {
            Ok((img, origin)) => Ok((DynamicImage::ImageRgb8(img), origin)),
            Err(direct) => {
                eprintln!("computer-use: direct capture failed ({direct:#}); using the screenshot portal");
                let path = portal::screenshot().with_context(|| format!("direct capture failed too ({direct:#})"))?;
                let img = image::open(&path).with_context(|| format!("decode {}", path.display()));
                let _ = std::fs::remove_file(&path);
                Ok((DynamicImage::ImageRgb8(img?.to_rgb8()), (0, 0)))
            }
        }
    }

    /// Returns PNG bytes. `region` = (x, y, w, h) in logical desktop coords; zoomed up to 4x.
    pub fn screenshot(
        &mut self,
        monitor: Option<&str>,
        region: Option<(i64, i64, i64, i64)>,
        max_w: u32,
        max_h: u32,
    ) -> Result<(Vec<u8>, Shot)> {
        let region = match monitor {
            Some(m) => {
                let mons = self.monitors()?;
                let r = find_monitor(&mons, m)?;
                Some((r.x as i64, r.y as i64, r.w as i64, r.h as i64))
            }
            None => region,
        };
        let (mut img, (ox, oy)) = self.capture()?;
        let (mut origin_x, mut origin_y) = (ox as f64, oy as f64);
        let zoomable = region.is_some();
        if let Some((x, y, w, h)) = region {
            // Desktop coords -> image pixels, clamped to the image.
            let (ix, iy) = ((x - ox as i64).max(0) as u32, (y - oy as i64).max(0) as u32);
            let w = (w.max(1) as u32).min(img.width().saturating_sub(ix));
            let h = (h.max(1) as u32).min(img.height().saturating_sub(iy));
            if w == 0 || h == 0 {
                bail!("region is outside the desktop");
            }
            img = img.crop_imm(ix, iy, w, h);
            (origin_x, origin_y) = (ox as f64 + ix as f64, oy as f64 + iy as f64);
        }
        let cap = if zoomable { 4.0 } else { 1.0 };
        let (max_w, max_h) = (max_w.max(16), max_h.max(16));
        let scale = (max_w as f64 / img.width() as f64).min(max_h as f64 / img.height() as f64).min(cap);
        if (scale - 1.0).abs() > 1e-6 {
            let (w, h) = ((img.width() as f64 * scale).round() as u32, (img.height() as f64 * scale).round() as u32);
            img = img.resize_exact(w.max(1), h.max(1), FilterType::Lanczos3);
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png)?;
        let shot = Shot { origin_x, origin_y, scale, width: img.width(), height: img.height() };
        Ok((buf.into_inner(), shot))
    }

    // -- input ----------------------------------------------------------------------------

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        let i = self.input()?;
        i.c.move_abs(x, y)
    }

    pub fn click(&mut self, x: f64, y: f64, button: &str, count: u32, modifiers: &str) -> Result<()> {
        let code = keys::button(button)?;
        let mods = if modifiers.trim().is_empty() { vec![] } else { keys::parse_combo(modifiers)? };
        let mut i = self.input()?;
        i.c.move_abs(x, y)?;
        sleep(ms(30));
        for &m in &mods {
            i.key(m, true)?;
        }
        for _ in 0..count.max(1) {
            i.button(code, true)?;
            sleep(ms(20));
            i.button(code, false)?;
            sleep(ms(60));
        }
        for &m in mods.iter().rev() {
            i.key(m, false)?;
        }
        Ok(())
    }

    pub fn drag(&mut self, from: (f64, f64), to: (f64, f64), button: &str) -> Result<()> {
        let code = keys::button(button)?;
        let mut i = self.input()?;
        i.c.move_abs(from.0, from.1)?;
        sleep(ms(30));
        i.button(code, true)?;
        let steps = 20;
        for n in 1..=steps {
            let t = n as f64 / steps as f64;
            i.c.move_abs(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)?;
            sleep(ms(15));
        }
        sleep(ms(50));
        i.button(code, false)
    }

    pub fn scroll(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let i = self.input()?;
        i.c.move_abs(x, y)?;
        sleep(ms(20));
        i.c.scroll(dx, dy)
    }

    pub fn key(&mut self, combo: &str, repeat: u32) -> Result<()> {
        let codes = keys::parse_combo(combo)?;
        let mut i = self.input()?;
        for _ in 0..repeat.max(1) {
            i.tap(&codes)?;
            sleep(ms(20));
        }
        Ok(())
    }

    /// Types ASCII as US-layout keystrokes; anything else is pasted via the clipboard.
    pub fn type_text(&mut self, text: &str, paste: PasteKey) -> Result<String> {
        let keys: Option<Vec<(u32, bool)>> = text.chars().map(keys::char_key).collect();
        let Some(keys) = keys else { return self.paste(text, paste) };
        if keys.len() > MAX_TYPED_CHARS {
            bail!(
                "text is {} characters; type at most {MAX_TYPED_CHARS} per call (split it), so a call never outlives \
                 the client's timeout",
                keys.len()
            );
        }
        let mut i = self.input()?;
        for (code, shift) in keys {
            if shift {
                i.key(keys::SHIFT, true)?;
            }
            i.key(code, true)?;
            i.key(code, false)?;
            if shift {
                i.key(keys::SHIFT, false)?;
            }
            sleep(ms(8));
        }
        Ok(format!("typed {} characters", text.chars().count()))
    }

    fn paste(&mut self, text: &str, paste: PasteKey) -> Result<String> {
        // Connect first: a pending approval must not cost the user's clipboard.
        self.client()?;
        set_clipboard(text).context("non-ASCII text is pasted via the clipboard")?;
        sleep(ms(80));
        let shift = match paste {
            PasteKey::CtrlV => false,
            PasteKey::CtrlShiftV => true,
            PasteKey::Auto => focused_is_terminal(),
        };
        let combo = if shift { "ctrl+shift+v" } else { "ctrl+v" };
        self.key(combo, 1)?;
        Ok(format!(
            "pasted {} characters via the clipboard ({combo}); the clipboard now holds that text",
            text.chars().count()
        ))
    }

    // -- clipboard ------------------------------------------------------------------------

    pub fn clipboard(&mut self, set: Option<&str>) -> Result<String> {
        match set {
            Some(t) => {
                set_clipboard(t)?;
                Ok(format!("clipboard set ({} characters)", t.chars().count()))
            }
            None => {
                let out = run_bounded(Command::new("wl-paste").args(["--no-newline", "--type", "text"]), None)
                    .context("read the clipboard (needs wl-clipboard)")?;
                Ok(String::from_utf8_lossy(&out).into_owned())
            }
        }
    }

    // -- windows --------------------------------------------------------------------------

    /// Windows with geometry translated to logical desktop coordinates.
    pub fn windows(&mut self) -> Result<Vec<Window>> {
        let mons = self.monitors().unwrap_or_default();
        let mut wins = Windows::connect()?.list();
        for w in &mut wins {
            for g in &mut w.geometry {
                if let Some(m) = mons.iter().find(|m| m.name == g.output) {
                    g.x += m.x;
                    g.y += m.y;
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

    /// The focused window, if the compositor reports one.
    pub fn focused(&mut self) -> Option<Window> {
        Windows::connect().ok()?.list().into_iter().find(|w| w.states.contains(&"focused"))
    }
}

pub fn find_monitor<'a>(mons: &'a [Monitor], m: &str) -> Result<&'a Monitor> {
    mons.iter()
        .find(|r| r.name.eq_ignore_ascii_case(m))
        .or_else(|| m.parse::<usize>().ok().and_then(|i| mons.get(i)))
        .ok_or_else(|| anyhow!("unknown monitor {m:?}; have {:?}", mons.iter().map(|r| &r.name).collect::<Vec<_>>()))
}

fn set_clipboard(text: &str) -> Result<()> {
    // wl-copy forks a server that holds the selection and exits once it is set.
    run_bounded(Command::new("wl-copy").args(["--type", "text/plain;charset=utf-8"]), Some(text.as_bytes()))
        .context("set the clipboard (needs wl-clipboard)")?;
    Ok(())
}

/// Run a helper with optional stdin and a deadline; its stdout on success.
fn run_bounded(cmd: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {name}"))?;
    if let Some(bytes) = input {
        let mut stdin = child.stdin.take().unwrap();
        let bytes = bytes.to_vec();
        // A writer thread, so a helper that stops reading cannot block us past the deadline.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{name} did not finish within {}s", CLIPBOARD_TIMEOUT.as_secs());
        }
        sleep(ms(10));
    };
    // wl-copy's forked server keeps the pipes open; do not wait on the readers then.
    let collect = |h: std::thread::JoinHandle<Vec<u8>>| {
        let end = Instant::now() + ms(300);
        while !h.is_finished() && Instant::now() < end {
            sleep(ms(5));
        }
        if h.is_finished() { h.join().unwrap_or_default() } else { Vec::new() }
    };
    let (out, err) = (collect(out), collect(err));
    if !status.success() {
        let msg = String::from_utf8_lossy(&err);
        // An empty clipboard is not an error for the caller.
        if name == "wl-paste" && msg.contains("No selection") {
            return Ok(Vec::new());
        }
        bail!("{name} failed ({status}): {}", msg.trim());
    }
    Ok(out)
}

const TERMINALS: &[&str] =
    &["foot", "wezterm", "kitty", "alacritty", "terminal", "konsole", "ghostty", "xterm", "term", "tilix", "rio"];

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
