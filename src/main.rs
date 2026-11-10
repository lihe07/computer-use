//! MCP server for desktop control on COSMIC/Wayland.

mod control_lock;
mod controller;
mod desktop;
mod ei;
mod hud;
mod keys;
mod portal;
mod portal_repair;
mod windows;
mod wl;
mod worker;

use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router, transport::stdio};
use serde::Deserialize;
use serde_json::Value;

use controller::{Controller, human};
use desktop::PasteKey;

const INSTRUCTIONS: &str = "\
Controls the user's real Wayland desktop (moves their actual cursor and keyboard focus).
Call `control` with enabled=true before any input tool: it shows the user an \"Agent is in
control\" HUD so they stop using the mouse and keyboard. Call it with enabled=false as soon
as you are done or need the user to act, to hand the desktop back. Control is released
automatically after 5 minutes without desktop calls.
Control is exclusive across all server processes for this desktop user. If another agent
holds it, control returns a busy error naming the holder; wait and retry rather than using
other tools. `screenshot`, `windows` and `monitors` only read and work without control.
Coordinates: by default x/y are pixels in the LAST screenshot image returned by this server
(space=\"image\"); the server maps them to the desktop, including crops and downscaling.
Take a fresh screenshot before acting, and again after to verify; do not send several input
actions in one batch without looking in between (if one call in a batch fails, the input
calls queued behind it are skipped). To hit small targets, screenshot a `region` around them
(it is upscaled) and click in that image; aim at the center of the target. count=3 on
`click` selects a line. To drag-select text, press on the LEFT edge of the first character
(terminals snap the selection start to the nearest cell boundary).
Use `windows` / `window` to find and focus apps instead of hunting in screenshots. Before
typing or pressing keys, pass `expect_window` (a title or app_id substring) so nothing is
sent if a different window has focus; input results report the focused window. Browser
extensions such as Vimium turn typed text into shortcuts: check that a text field has focus.
Pass space=\"desktop\" for logical desktop coordinates (see `monitors`). `clipboard` reads or
sets the clipboard text. If input calls report that the desktop is waiting for the user to
allow remote control, ask the user to approve the dialog; do not retry in a loop.";

/// Hard ceiling for the `control` tool call.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Input calls that arrive this soon after (or before) another call failed are skipped:
/// they were sent in the same batch, planned without knowing about the failure.
const BATCH_WINDOW: Duration = Duration::from_millis(1500);
const MAX_REPEAT: u32 = 500;
const MAX_CLICKS: u32 = 10;

type Res = Result<CallToolResult, ErrorData>;

/// The binary this process runs, for spawning the worker and HUD. `/proc/self/exe`
/// keeps working after the file on disk is replaced or deleted by a reinstall, where
/// `current_exe()` (a path) gave "No such file or directory".
pub(crate) fn self_exe() -> std::path::PathBuf {
    if std::fs::metadata("/proc/self/exe").is_ok() {
        "/proc/self/exe".into()
    } else {
        std::env::current_exe().unwrap_or_else(|_| "computer-use".into())
    }
}

fn with_notes(mut text: String, notes: &[String]) -> String {
    for n in notes {
        text.push('\n');
        text.push_str(n);
    }
    text
}

fn ok_text(text: String, notes: &[String]) -> Res {
    Ok(CallToolResult::success(vec![ContentBlock::text(with_notes(text, notes))]))
}

fn fail(e: impl std::fmt::Display) -> Res {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]))
}

/// What a tool does to the desktop, which decides gating and batch skipping.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Reads only; allowed without control and never skipped.
    Read,
    /// Changes the desktop: needs control, and is skipped right after a failure.
    Act,
}

#[derive(Clone)]
struct Server {
    desk: Arc<Mutex<Controller>>,
    /// The most recent failed call: when it finished and what it was.
    last_failure: Arc<Mutex<Option<(Instant, String)>>>,
    /// How long a call waits for an earlier call that still holds the desktop.
    lock_wait: Duration,
    #[allow(dead_code)] // read by #[tool_handler]
    tool_router: ToolRouter<Self>,
}

/// A tool's outcome plus warnings for the agent (HUD missing, stale screenshot, ...).
struct Done<T> {
    value: T,
    notes: Vec<String>,
}

impl Server {
    /// Run `f` with the desktop on a blocking thread (input uses sleeps; portal calls block_on).
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Controller) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let desk = self.desk.clone();
        let wait = self.lock_wait;
        tokio::task::spawn_blocking(move || {
            // Never block forever behind an earlier call: report it instead.
            let deadline = Instant::now() + wait;
            let mut d = loop {
                match desk.try_lock() {
                    Ok(g) => break g,
                    Err(TryLockError::Poisoned(e)) => break e.into_inner(),
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(TryLockError::WouldBlock) => anyhow::bail!(
                        "an earlier desktop call is still running; try again shortly"
                    ),
                }
            };
            d.touch();
            f(&mut d)
        })
        .await?
    }

    /// Run a tool body: gate on control for `Kind::Act`, skip input queued behind a
    /// failure, collect warnings, and remember failures for the calls behind this one.
    async fn run<T: Send + 'static>(
        &self,
        tool: &'static str,
        kind: Kind,
        f: impl FnOnce(&mut Controller) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<Done<T>> {
        let arrived = Instant::now();
        let failures = self.last_failure.clone();
        let r = self
            .with(move |d| {
                if kind == Kind::Act {
                    d.require_control()?;
                    let prior = failures.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Some((_, what)) =
                        prior.filter(|(at, _)| *at > arrived || arrived.duration_since(*at) < BATCH_WINDOW)
                    {
                        anyhow::bail!(
                            "skipped, nothing was done: it was sent together with `{what}`, which failed just \
                             before it. Read that error, take a screenshot, and decide again"
                        );
                    }
                }
                let value = f(d)?;
                Ok(Done { value, notes: d.notes() })
            })
            .await;
        if let Err(e) = &r {
            let short: String = format!("{e:#}").chars().take(160).collect();
            *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), format!("{tool}: {short}")));
        }
        r
    }

    fn new_with(desk: Controller) -> Self {
        let desk = Arc::new(Mutex::new(desk));
        spawn_reaper(Arc::downgrade(&desk));
        Self {
            desk,
            last_failure: Arc::default(),
            lock_wait: Duration::from_secs(10),
            tool_router: Self::tool_router(),
        }
    }
}

/// Periodic housekeeping (idle release, worker reaping) for as long as the server lives.
fn spawn_reaper(desk: Weak<Mutex<Controller>>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some(desk) = desk.upgrade() else { return };
            // Never wait behind a running call; the next tick will do.
            if let Ok(mut d) = desk.try_lock() {
                d.tick();
            }
        }
    });
}

fn is_desktop(space: &Option<String>) -> bool {
    space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("desktop"))
}

/// " (focus: \"Title\" app)" from a worker input reply.
fn focus_suffix(v: &Value) -> String {
    match v.get("focused") {
        Some(Value::Object(w)) => {
            let s = |k: &str| w.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            format!("; focused window: {:?} ({})", s("title"), s("app_id"))
        }
        Some(Value::Null) => "; no window has keyboard focus".into(),
        _ => String::new(),
    }
}

/// A note when image coordinates came from an old screenshot.
fn age_note(age: Option<Duration>) -> Option<String> {
    let age = age?;
    (age > Duration::from_secs(30)).then(|| {
        format!("note: mapped through a screenshot taken {} ago; take a fresh one if the screen may have changed", human(age))
    })
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ControlP {
    /// true to take control (shows the HUD; required before input tools), false to hand back
    #[serde(alias = "enable", alias = "on")]
    enabled: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScreenshotP {
    /// Monitor name (e.g. "HDMI-A-1") or index from `monitors`, to capture one screen.
    #[serde(alias = "output", alias = "screen")]
    monitor: Option<String>,
    /// [x, y, w, h] in logical desktop coords to zoom into (upscaled up to 4x).
    #[serde(alias = "rect", alias = "zoom")]
    region: Option<Vec<i64>>,
    /// Max returned image width (default 1920).
    max_width: Option<u32>,
    /// Max returned image height (default 1200).
    max_height: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ClickP {
    x: f64,
    y: f64,
    /// left (default), right, middle, back, forward
    button: Option<String>,
    /// 2 for double-click, 3 for triple-click (selects a line); at most 10
    #[serde(alias = "clicks")]
    count: Option<u32>,
    /// Held during the click, e.g. "ctrl" or "ctrl+shift"
    #[serde(alias = "modifier")]
    modifiers: Option<String>,
    /// "image" (default: pixels of last screenshot) or "desktop"
    space: Option<String>,
    /// Only click if the focused window's title or app_id contains this (case-insensitive)
    expect_window: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PointP {
    x: f64,
    y: f64,
    /// "image" (default) or "desktop"
    space: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DragP {
    #[serde(alias = "start_x", alias = "from_x")]
    x1: f64,
    #[serde(alias = "start_y", alias = "from_y")]
    y1: f64,
    #[serde(alias = "end_x", alias = "to_x")]
    x2: f64,
    #[serde(alias = "end_y", alias = "to_y")]
    y2: f64,
    button: Option<String>,
    /// "image" (default) or "desktop"
    space: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScrollP {
    x: f64,
    y: f64,
    /// Wheel notches; positive scrolls down (default 3)
    #[serde(alias = "delta", alias = "amount")]
    dy: Option<f64>,
    /// Wheel notches; positive scrolls right
    dx: Option<f64>,
    /// "image" (default) or "desktop"
    space: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyP {
    /// Key or chord: "enter", "ctrl+c", "super", "alt+tab", "ctrl+shift+t", "pagedown"
    #[serde(alias = "keys", alias = "key", alias = "chord", alias = "shortcut")]
    combo: String,
    /// Press it this many times (at most 500)
    #[serde(alias = "count", alias = "times")]
    repeat: Option<u32>,
    /// Only press if the focused window's title or app_id contains this (case-insensitive)
    expect_window: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TypeP {
    /// Text to type (ASCII is typed, at most 3000 characters per call; other text is pasted)
    #[serde(alias = "string", alias = "value", alias = "content")]
    text: String,
    /// How non-ASCII text is pasted: "auto" (default: ctrl+shift+v in terminals, else ctrl+v),
    /// "ctrl+v" or "ctrl+shift+v"
    paste_key: Option<String>,
    /// Only type if the focused window's title or app_id contains this (case-insensitive)
    expect_window: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ClipboardP {
    /// Text to put on the clipboard; omit to read the clipboard instead
    #[serde(alias = "text", alias = "value")]
    set: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WindowP {
    /// Window id from `windows`, or a unique case-insensitive substring of its title or app_id
    #[serde(alias = "id", alias = "title", alias = "app_id", alias = "name", alias = "query")]
    window: String,
    /// focus (default), close, maximize, unmaximize, minimize, unminimize, fullscreen, unfullscreen
    action: Option<String>,
}

#[tool_router]
impl Server {
    fn new() -> Self {
        Self::new_with(Controller::new())
    }

    #[tool(description = "Take (enabled=true) or release (enabled=false) control of the desktop. \
        Control is exclusive across agents: returns a busy error naming the holder if another server holds it. \
        Input tools require control; while held, the user sees an \"Agent is in control\" HUD. \
        Released automatically after 5 minutes without desktop calls.")]
    async fn control(&self, Parameters(p): Parameters<ControlP>) -> Res {
        let on = p.enabled;
        // `with` is bounded already (mutex wait, HUD and portal timeouts); this is the backstop.
        let r = match tokio::time::timeout(CONTROL_TIMEOUT, self.with(move |d| d.set_control(on))).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("control timed out after {}s", CONTROL_TIMEOUT.as_secs())),
        };
        match r {
            Ok(None) if on => ok_text("in control; HUD shown".into(), &[]),
            Ok(Some(e)) => ok_text(
                format!(
                    "in control, but the HUD could not be shown ({e}). WARNING: the user cannot see that you are \
                     using the desktop; tell them before acting"
                ),
                &[],
            ),
            Ok(None) => ok_text("control released; HUD hidden".into(), &[]),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "List monitors as logical desktop rectangles (left to right). Works without control.")]
    async fn monitors(&self) -> Res {
        match self.run("monitors", Kind::Read, |d| d.monitors()).await {
            Ok(Done { value: m, notes }) => ok_text(
                m.iter()
                    .enumerate()
                    .map(|(i, r)| format!("{i}: {}  x={} y={} {}x{} scale={}", r.name, r.x, r.y, r.w, r.h, r.scale))
                    .collect::<Vec<_>>()
                    .join("\n"),
                &notes,
            ),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Capture the desktop (or one monitor, or a zoomed region). Later x/y (space=\"image\") \
        refer to pixels of this image. Works without control.")]
    async fn screenshot(&self, Parameters(p): Parameters<ScreenshotP>) -> Res {
        let region = match p.region.as_deref() {
            None => None,
            Some([x, y, w, h]) if *w > 0 && *h > 0 => Some((*x, *y, *w, *h)),
            Some(_) => return fail("region must be [x, y, w, h] with positive width and height"),
        };
        let r = self
            .run("screenshot", Kind::Read, move |d| {
                d.screenshot(p.monitor.as_deref(), region, p.max_width.unwrap_or(1920), p.max_height.unwrap_or(1200))
            })
            .await;
        match r {
            Ok(Done { value: (png, s), notes }) => Ok(CallToolResult::success(vec![
                ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(png), "image/png"),
                ContentBlock::text(with_notes(
                    format!(
                        "image {}x{}; desktop = ({}, {}) + image_px / {:.4}",
                        s.width, s.height, s.origin_x, s.origin_y, s.scale
                    ),
                    &notes,
                )),
            ])),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Click at (x, y). count=2 for double-click, 3 for triple-click. Reports the focused window afterwards.")]
    async fn click(&self, Parameters(p): Parameters<ClickP>) -> Res {
        let r = self
            .run("click", Kind::Act, move |d| {
                let ((x, y), age) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                let button = p.button.unwrap_or_else(|| "left".into());
                let count = p.count.unwrap_or(1).clamp(1, MAX_CLICKS);
                let v = d.click(x, y, &button, count, p.modifiers.as_deref().unwrap_or(""), p.expect_window)?;
                let mut msg = format!("clicked {button} x{count} at desktop ({x:.0}, {y:.0}){}", focus_suffix(&v));
                if let Some(n) = age_note(age) {
                    msg = format!("{msg}\n{n}");
                }
                Ok(msg)
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Move the pointer without clicking (hover).")]
    async fn move_pointer(&self, Parameters(p): Parameters<PointP>) -> Res {
        let r = self
            .run("move_pointer", Kind::Act, move |d| {
                let ((x, y), _) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                d.move_to(x, y)?;
                Ok(format!("moved to desktop ({x:.0}, {y:.0})"))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Press at (x1, y1), move to (x2, y2), release.")]
    async fn drag(&self, Parameters(p): Parameters<DragP>) -> Res {
        let r = self
            .run("drag", Kind::Act, move |d| {
                let desk = is_desktop(&p.space);
                let (a, _) = d.resolve(p.x1, p.y1, desk)?;
                let (b, _) = d.resolve(p.x2, p.y2, desk)?;
                d.drag(a, b, p.button.as_deref().unwrap_or("left"))?;
                Ok(format!("dragged desktop ({:.0}, {:.0}) -> ({:.0}, {:.0})", a.0, a.1, b.0, b.1))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Scroll at (x, y) by wheel notches.")]
    async fn scroll(&self, Parameters(p): Parameters<ScrollP>) -> Res {
        let r = self
            .run("scroll", Kind::Act, move |d| {
                let ((x, y), _) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                let (dx, dy) = (p.dx.unwrap_or(0.0).clamp(-100.0, 100.0), p.dy.unwrap_or(3.0).clamp(-100.0, 100.0));
                d.scroll(x, y, dx, dy)?;
                Ok(format!("scrolled ({dx}, {dy}) at desktop ({x:.0}, {y:.0})"))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Press a key or chord in the focused window. Pass expect_window to refuse if the wrong window has focus.")]
    async fn key(&self, Parameters(p): Parameters<KeyP>) -> Res {
        let r = self
            .run("key", Kind::Act, move |d| {
                let n = p.repeat.unwrap_or(1).clamp(1, MAX_REPEAT);
                keys::parse_combo(&p.combo)?; // report bad names before touching the desktop
                let v = d.key(&p.combo, n, p.expect_window)?;
                Ok(format!(
                    "pressed {}{}{}",
                    p.combo,
                    if n > 1 { format!(" x{n}") } else { String::new() },
                    focus_suffix(&v)
                ))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Type text into the focused window (US layout). Non-ASCII is pasted via the clipboard. \
        Pass expect_window to refuse if the wrong window has focus.")]
    async fn type_text(&self, Parameters(p): Parameters<TypeP>) -> Res {
        let paste = match p.paste_key.as_deref().map(str::to_lowercase).as_deref() {
            None | Some("auto") => PasteKey::Auto,
            Some("ctrl+v") => PasteKey::CtrlV,
            Some("ctrl+shift+v") => PasteKey::CtrlShiftV,
            Some(o) => return fail(format!("unknown paste_key {o:?}")),
        };
        if p.text.is_empty() {
            return fail("text is empty");
        }
        let r = self
            .run("type_text", Kind::Act, move |d| {
                let v = d.type_text(&p.text, paste, p.expect_window)?;
                let done = v.get("done").and_then(Value::as_str).unwrap_or("typed").to_owned();
                Ok(format!("{done}{}", focus_suffix(&v)))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "Read the clipboard text, or set it with `set`.")]
    async fn clipboard(&self, Parameters(p): Parameters<ClipboardP>) -> Res {
        let r = self.run("clipboard", Kind::Act, move |d| d.clipboard(p.set)).await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }

    #[tool(description = "List open windows: id, title, app_id, states (focused/maximized/...) and \
        geometry in logical desktop coords. Works without control.")]
    async fn windows(&self) -> Res {
        let r = self.run("windows", Kind::Read, |d| d.windows()).await;
        match r {
            Ok(Done { value: w, notes }) => ok_text(serde_json::to_string_pretty(&w).unwrap_or_default(), &notes),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Focus, close, maximize, minimize or fullscreen a window by id or title/app_id substring.")]
    async fn window(&self, Parameters(p): Parameters<WindowP>) -> Res {
        let action = p.action.unwrap_or_else(|| "focus".into()).to_lowercase();
        let r = self
            .run("window", Kind::Act, move |d| {
                let w = d.window_action(&p.window, &action)?;
                let f = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                Ok(format!("{action}: {} ({}, id {})", f("title"), f("app_id"), f("id")))
            })
            .await;
        r.map_or_else(fail, |d| ok_text(d.value, &d.notes))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("computer-use", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// MCP hosts may launch us with a scrubbed env; recover the user session's bus and display.
fn fix_env() {
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
        // An explicit display that is not a live socket (a stale config) is as bad as none.
        let explicit = std::env::var_os("WAYLAND_DISPLAY").filter(|d| {
            let p = std::path::Path::new(&runtime).join(d);
            let live = std::os::unix::net::UnixStream::connect(&p).is_ok();
            if !live {
                eprintln!("computer-use: WAYLAND_DISPLAY={} is not a live socket; discovering the session", p.display());
            }
            live
        });
        if explicit.is_none() {
            std::env::remove_var("WAYLAND_DISPLAY");
            let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(1000);
            match portal_repair::discover(uid, std::path::Path::new(&runtime)) {
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

fn main() -> anyhow::Result<()> {
    // Repair must inspect the caller's display before fix_env guesses one.
    if std::env::args().nth(1).as_deref() == Some("--repair-portal") {
        return portal_repair::run();
    }
    fix_env();
    match std::env::args().nth(1).as_deref() {
        Some(hud::ARG) => return hud::run(),
        Some(worker::ARG) => return worker::run(),
        _ => {}
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let service = Server::new().serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Parameters<T> {
        Parameters(serde_json::from_value(value).unwrap())
    }

    #[tokio::test]
    async fn every_desktop_tool_requires_control() {
        let server = Server::new();
        let point = json!({"x": 0, "y": 0, "space": "desktop"});
        let results = vec![
            server.click(params(point.clone())).await,
            server.move_pointer(params(point.clone())).await,
            server.drag(params(json!({"x1": 0, "y1": 0, "x2": 1, "y2": 1, "space": "desktop"}))).await,
            server.scroll(params(point)).await,
            server.key(params(json!({"combo": "enter"}))).await,
            server.type_text(params(json!({"text": "test"}))).await,
            server.clipboard(params(json!({}))).await,
            server.window(params(json!({"window": "test"}))).await,
        ];
        for result in results {
            let result = result.unwrap();
            assert_eq!(result.is_error, Some(true));
            assert!(serde_json::to_string(&result).unwrap().contains("not in control"));
        }
        // Release is safe even for a server that never acquired control.
        assert_ne!(server.control(params(json!({"enabled": false}))).await.unwrap().is_error, Some(true));
    }

    #[tokio::test]
    async fn input_queued_behind_a_failure_is_skipped() {
        let path = crate::control_lock::tests::TestPath::new();
        let mut c = Controller::new();
        c.hold_for_test(crate::control_lock::ControlLock::at(&path.0).unwrap());
        let server = Server::new_with(c);
        // A call that failed after this one was sent (same batch, run before it).
        *server.last_failure.lock().unwrap() = Some((Instant::now() + Duration::from_secs(1), "click: boom".into()));
        for r in [
            server.key(params(json!({"combo": "enter"}))).await.unwrap(),
            server.type_text(params(json!({"text": "hello"}))).await.unwrap(),
            server.window(params(json!({"window": "x"}))).await.unwrap(),
        ] {
            assert_eq!(r.is_error, Some(true));
            let t = serde_json::to_string(&r).unwrap();
            assert!(t.contains("skipped") && t.contains("click: boom"), "{t}");
        }
        // Releasing is never skipped.
        let r = server.control(params(json!({"enabled": false}))).await.unwrap();
        assert_ne!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn unknown_key_names_fail_before_touching_the_desktop() {
        let path = crate::control_lock::tests::TestPath::new();
        let mut c = Controller::new();
        c.hold_for_test(crate::control_lock::ControlLock::at(&path.0).unwrap());
        let server = Server::new_with(c);
        let r = server.key(params(json!({"combo": "ctrl+nosuchkey"}))).await.unwrap();
        assert!(serde_json::to_string(&r).unwrap().contains("unknown key"));
    }

    #[tokio::test]
    async fn parameter_aliases_are_accepted() {
        let k: KeyP = serde_json::from_value(json!({"keys": "ctrl+c", "count": 2})).unwrap();
        assert_eq!((k.combo.as_str(), k.repeat), ("ctrl+c", Some(2)));
        let w: WindowP = serde_json::from_value(json!({"id": "abc"})).unwrap();
        assert_eq!(w.window, "abc");
        let c: ClickP = serde_json::from_value(json!({"x": 1, "y": 2, "clicks": 3})).unwrap();
        assert_eq!(c.count, Some(3));
    }

    #[tokio::test]
    async fn control_does_not_hang_behind_a_stuck_call() {
        let mut server = Server::new();
        server.lock_wait = Duration::from_millis(100);
        let held = server.desk.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _g = held.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let started = Instant::now();
        for on in [true, false] {
            let r = server.control(params(json!({ "enabled": on }))).await.unwrap();
            assert_eq!(r.is_error, Some(true));
            assert!(serde_json::to_string(&r).unwrap().contains("still running"));
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        release_tx.send(()).unwrap();
        stuck.join().unwrap();
        // Once the earlier call finishes, release works again.
        assert_ne!(server.control(params(json!({"enabled": false}))).await.unwrap().is_error, Some(true));
    }
}
