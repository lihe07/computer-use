//! MCP server for desktop control on COSMIC/Wayland.

mod desktop;
mod ei;
mod keys;
mod portal;
mod windows;

use std::sync::{Arc, Mutex};

use base64::Engine;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router, transport::stdio};
use serde::Deserialize;

use desktop::{Desktop, PasteKey};

const INSTRUCTIONS: &str = "\
Controls the user's real Wayland desktop (moves their actual cursor and keyboard focus).
Coordinates: by default x/y are pixels in the LAST screenshot image returned by this server
(space=\"image\"); the server maps them to the desktop, including crops and downscaling.
Take a fresh screenshot before acting, and again after to verify. To hit small targets,
screenshot a `region` around them (it is upscaled) and click in that image; aim at the
center of the target. To drag-select text, press on the LEFT edge of the first character
(terminals snap the selection start to the nearest cell boundary). Use `windows` / `window` to find and focus apps instead of hunting
in screenshots. Pass space=\"desktop\" for logical desktop coordinates (see `monitors`).";

type Res = Result<CallToolResult, ErrorData>;

fn text(s: impl Into<String>) -> Res {
    Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
}

fn fail(e: impl std::fmt::Display) -> Res {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]))
}

#[derive(Clone)]
struct Server {
    desk: Arc<Mutex<Desktop>>,
    #[allow(dead_code)] // read by #[tool_handler]
    tool_router: ToolRouter<Self>,
}

impl Server {
    /// Run `f` with the desktop on a blocking thread (input uses sleeps; portal calls block_on).
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Desktop) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let desk = self.desk.clone();
        tokio::task::spawn_blocking(move || {
            let mut d = desk.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut d)
        })
        .await?
    }
}

fn is_desktop(space: &Option<String>) -> bool {
    space.as_deref() == Some("desktop")
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScreenshotP {
    /// Monitor name (e.g. "HDMI-A-1") or index from `monitors`, to capture one screen.
    monitor: Option<String>,
    /// [x, y, w, h] in logical desktop coords to zoom into (upscaled up to 4x).
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
    /// 2 for double-click
    count: Option<u32>,
    /// Held during the click, e.g. "ctrl" or "ctrl+shift"
    modifiers: Option<String>,
    /// "image" (default: pixels of last screenshot) or "desktop"
    space: Option<String>,
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
    x1: f64,
    y1: f64,
    x2: f64,
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
    dy: Option<f64>,
    /// Wheel notches; positive scrolls right
    dx: Option<f64>,
    /// "image" (default) or "desktop"
    space: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyP {
    /// Key or chord: "enter", "ctrl+c", "super", "alt+tab", "ctrl+shift+t"
    combo: String,
    repeat: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TypeP {
    text: String,
    /// How non-ASCII text is pasted: "auto" (default: ctrl+shift+v in terminals, else ctrl+v),
    /// "ctrl+v" or "ctrl+shift+v"
    paste_key: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WindowP {
    /// Window id from `windows`, or a unique case-insensitive substring of its title or app_id
    window: String,
    /// focus (default), close, maximize, unmaximize, minimize, unminimize, fullscreen, unfullscreen
    action: Option<String>,
}

#[tool_router]
impl Server {
    fn new() -> Self {
        Self { desk: Arc::new(Mutex::new(Desktop::new())), tool_router: Self::tool_router() }
    }

    #[tool(description = "List monitors as logical desktop rectangles (left to right).")]
    async fn monitors(&self) -> Res {
        match self.with(|d| d.monitors()).await {
            Ok(m) => text(
                m.iter()
                    .enumerate()
                    .map(|(i, r)| format!("{i}: {}  x={} y={} {}x{} scale={}", r.name, r.x, r.y, r.w, r.h, r.scale))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Capture the desktop. Later x/y (space=\"image\") refer to pixels of this image.")]
    async fn screenshot(&self, Parameters(p): Parameters<ScreenshotP>) -> Res {
        let region = match p.region.as_deref() {
            None => None,
            Some([x, y, w, h]) => Some((*x, *y, *w, *h)),
            Some(_) => return fail("region must be [x, y, w, h]"),
        };
        let r = self
            .with(move |d| {
                d.screenshot(p.monitor.as_deref(), region, p.max_width.unwrap_or(1920), p.max_height.unwrap_or(1200))
            })
            .await;
        match r {
            Ok((png, s)) => Ok(CallToolResult::success(vec![
                ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(png), "image/png"),
                ContentBlock::text(format!(
                    "image {}x{}; desktop = ({}, {}) + image_px / {:.4}",
                    s.width, s.height, s.origin_x, s.origin_y, s.scale
                )),
            ])),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Click at (x, y). count=2 for double-click.")]
    async fn click(&self, Parameters(p): Parameters<ClickP>) -> Res {
        let r = self
            .with(move |d| {
                let (x, y) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                let button = p.button.unwrap_or_else(|| "left".into());
                let count = p.count.unwrap_or(1);
                d.click(x, y, &button, count, p.modifiers.as_deref().unwrap_or(""))?;
                Ok(format!("clicked {button} x{count} at desktop ({x:.0}, {y:.0})"))
            })
            .await;
        r.map_or_else(fail, text)
    }

    #[tool(description = "Move the pointer without clicking (hover).")]
    async fn move_pointer(&self, Parameters(p): Parameters<PointP>) -> Res {
        let r = self
            .with(move |d| {
                let (x, y) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                d.move_to(x, y)?;
                Ok(format!("moved to desktop ({x:.0}, {y:.0})"))
            })
            .await;
        r.map_or_else(fail, text)
    }

    #[tool(description = "Press at (x1, y1), move to (x2, y2), release.")]
    async fn drag(&self, Parameters(p): Parameters<DragP>) -> Res {
        let r = self
            .with(move |d| {
                let desk = is_desktop(&p.space);
                let a = d.resolve(p.x1, p.y1, desk)?;
                let b = d.resolve(p.x2, p.y2, desk)?;
                d.drag(a, b, p.button.as_deref().unwrap_or("left"))?;
                Ok(format!("dragged desktop ({:.0}, {:.0}) -> ({:.0}, {:.0})", a.0, a.1, b.0, b.1))
            })
            .await;
        r.map_or_else(fail, text)
    }

    #[tool(description = "Scroll at (x, y) by wheel notches.")]
    async fn scroll(&self, Parameters(p): Parameters<ScrollP>) -> Res {
        let r = self
            .with(move |d| {
                let (x, y) = d.resolve(p.x, p.y, is_desktop(&p.space))?;
                let (dx, dy) = (p.dx.unwrap_or(0.0), p.dy.unwrap_or(3.0));
                d.scroll(x, y, dx, dy)?;
                Ok(format!("scrolled ({dx}, {dy}) at desktop ({x:.0}, {y:.0})"))
            })
            .await;
        r.map_or_else(fail, text)
    }

    #[tool(description = "Press a key or chord in the focused window.")]
    async fn key(&self, Parameters(p): Parameters<KeyP>) -> Res {
        let r = self
            .with(move |d| {
                let n = p.repeat.unwrap_or(1);
                d.key(&p.combo, n)?;
                Ok(format!("pressed {}{}", p.combo, if n > 1 { format!(" x{n}") } else { String::new() }))
            })
            .await;
        r.map_or_else(fail, text)
    }

    #[tool(description = "Type text into the focused window (US layout). Non-ASCII is pasted via the clipboard.")]
    async fn type_text(&self, Parameters(p): Parameters<TypeP>) -> Res {
        let paste = match p.paste_key.as_deref() {
            None | Some("auto") => PasteKey::Auto,
            Some("ctrl+v") => PasteKey::CtrlV,
            Some("ctrl+shift+v") => PasteKey::CtrlShiftV,
            Some(o) => return fail(format!("unknown paste_key {o:?}")),
        };
        self.with(move |d| d.type_text(&p.text, paste)).await.map_or_else(fail, text)
    }

    #[tool(description = "List open windows: id, title, app_id, states (focused/maximized/...) and \
        geometry in logical desktop coords.")]
    async fn windows(&self) -> Res {
        let r = self.with(|d| d.windows()).await;
        match r {
            Ok(w) => text(serde_json::to_string_pretty(&w).unwrap_or_default()),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Focus, close, maximize, minimize or fullscreen a window by id or title/app_id substring.")]
    async fn window(&self, Parameters(p): Parameters<WindowP>) -> Res {
        let action = p.action.unwrap_or_else(|| "focus".into());
        let r = self
            .with(move |d| {
                let w = d.window_action(&p.window, &action)?;
                Ok(format!("{action}: {} ({}, id {})", w.title, w.app_id, w.id))
            })
            .await;
        r.map_or_else(fail, text)
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
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            let sock = std::fs::read_dir(&runtime).ok().and_then(|it| {
                it.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                    .min()
            });
            if let Some(s) = sock {
                std::env::set_var("WAYLAND_DISPLAY", s);
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    fix_env();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let service = Server::new().serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
