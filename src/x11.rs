//! Direct X11 capture, XTEST input and EWMH window management.

use anyhow::{Context, Result, bail};
use image::{DynamicImage, Rgb, RgbImage};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::image::{Image, PixelLayout};
use x11rb::protocol::xproto::*;
use x11rb::protocol::{ErrorKind, randr, shape, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::desktop::DesktopBackend;
use crate::ei::Region;
use crate::windows::{Geometry, Window, find_window};

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_CHANGE_STATE,
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_CLOSE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_MAXIMIZED_VERT,
    }
}

pub struct X11 {
    conn: RustConnection,
    screen: usize,
    atoms: Atoms,
    scroll_remainder: [f64; 2],
}

impl X11 {
    pub fn open() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connect to X11 DISPLAY")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        Ok(Self { conn, screen, atoms, scroll_remainder: [0.0; 2] })
    }

    fn screen(&self) -> &Screen {
        &self.conn.setup().roots[self.screen]
    }

    fn root(&self) -> u32 {
        self.screen().root
    }

    fn input(&self, event: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        xtest::fake_input(&self.conn, event, detail, CURRENT_TIME, self.root(), x, y, 0)?.check()?;
        Ok(())
    }

    fn pointer_button(&self, button: u8, press: bool) -> Result<()> {
        self.input(if press { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, button, 0, 0)
    }

    fn property(&self, xid: u32, name: impl Into<Atom>, ty: impl Into<Atom>) -> Result<GetPropertyReply, ReplyError> {
        self.conn.get_property(false, xid, name, ty, 0, 4096)?.reply()
    }

    fn window(&self, xid: u32, focused: Option<u32>, monitors: &[Region]) -> Result<Window, ReplyError> {
        let a = &self.atoms;
        let c = &self.conn;
        let mut title = self.property(xid, a._NET_WM_NAME, a.UTF8_STRING)?.value;
        if title.is_empty() {
            title = self.property(xid, AtomEnum::WM_NAME, AtomEnum::STRING)?.value;
        }
        let class = self.property(xid, AtomEnum::WM_CLASS, AtomEnum::STRING)?.value;
        let mut class = class.split(|&byte| byte == 0).filter(|part| !part.is_empty());
        let app_id = String::from_utf8_lossy(class.next_back().unwrap_or_default()).into_owned();
        let state = self.property(xid, a._NET_WM_STATE, AtomEnum::ATOM)?;
        let has = |atom| state.value32().is_some_and(|mut v| v.any(|value| value == atom));
        let states = [
            (focused == Some(xid), "focused"),
            (has(a._NET_WM_STATE_HIDDEN), "minimized"),
            (has(a._NET_WM_STATE_MAXIMIZED_HORZ) && has(a._NET_WM_STATE_MAXIMIZED_VERT), "maximized"),
            (has(a._NET_WM_STATE_FULLSCREEN), "fullscreen"),
        ]
        .into_iter()
        .filter_map(|(set, name)| set.then_some(name))
        .collect();
        let g = c.get_geometry(xid)?.reply()?;
        let p = c.translate_coordinates(xid, self.root(), 0, 0)?.reply()?;
        let (x, y, width, height) = (i32::from(p.dst_x), i32::from(p.dst_y), i32::from(g.width), i32::from(g.height));
        let output = monitors
            .iter()
            .max_by_key(|m| {
                let (mx, my) = (m.x as i32, m.y as i32);
                let w = (x + width).min(mx + m.w as i32) - x.max(mx);
                let h = (y + height).min(my + m.h as i32) - y.max(my);
                i64::from(w.max(0)) * i64::from(h.max(0))
            })
            .map(|m| m.name.clone())
            .unwrap_or_default();
        Ok(Window {
            id: format!("0x{xid:x}"),
            title: String::from_utf8_lossy(&title).into_owned(),
            app_id,
            states,
            geometry: vec![Geometry { output, x, y, width, height }],
        })
    }
}

impl DesktopBackend for X11 {
    fn capture(&self) -> Result<DynamicImage> {
        let c = &self.conn;
        let root = self.root();
        let geometry = c.get_geometry(root)?.reply()?;
        let (w, h) = (geometry.width, geometry.height);
        let (shot, visual_id) = Image::get(c, root, 0, 0, w, h)?;
        let visual = self
            .screen()
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == visual_id)
            .context("unknown X11 root visual")?;
        let layout = PixelLayout::from_visual_type(*visual).context("unsupported X11 root visual")?;
        Ok(RgbImage::from_fn(w.into(), h.into(), |x, y| {
            let (r, g, b) = layout.decode(shot.get_pixel(x as u16, y as u16));
            Rgb([r, g, b].map(|channel| (channel >> 8) as u8))
        })
        .into())
    }

    fn monitors(&mut self) -> Result<Vec<Region>> {
        let c = &self.conn;
        randr::get_monitors(c, self.root(), true)?
            .reply()?
            .monitors
            .into_iter()
            .map(|m| {
                let name = c.get_atom_name(m.name)?.reply()?.name;
                Ok(Region {
                    name: String::from_utf8_lossy(&name).into_owned(),
                    x: m.x.max(0) as u32,
                    y: m.y.max(0) as u32,
                    w: m.width.into(),
                    h: m.height.into(),
                    scale: 1.0,
                })
            })
            .collect()
    }

    fn windows(&mut self) -> Result<Vec<Window>> {
        let a = &self.atoms;
        let root = self.root();
        let clients = self.property(root, a._NET_CLIENT_LIST, AtomEnum::WINDOW)?;
        let focused = self.property(root, a._NET_ACTIVE_WINDOW, AtomEnum::WINDOW)?.value32().and_then(|mut v| v.next());
        let monitors = self.monitors()?;
        clients
            .value32()
            .into_iter()
            .flatten()
            .filter_map(|xid| match self.window(xid, focused, &monitors) {
                // The client may have closed since _NET_CLIENT_LIST was read.
                Err(ReplyError::X11Error(e)) if matches!(e.error_kind, ErrorKind::Window | ErrorKind::Drawable) => None,
                result => Some(result.map_err(Into::into)),
            })
            .collect()
    }

    fn window_action(&mut self, query: &str, action: &str) -> Result<Window> {
        let windows = self.windows()?;
        let w = find_window(&query.to_ascii_lowercase(), windows.iter().map(|w| (w, w)))?;
        let xid = u32::from_str_radix(&w.id[2..], 16)?;
        let a = &self.atoms;
        let add = u32::from(matches!(action, "maximize" | "fullscreen"));
        let (message, data) = match action {
            "activate" | "focus" | "unminimize" => (a._NET_ACTIVE_WINDOW, [2, CURRENT_TIME, 0, 0, 0]),
            "close" => (a._NET_CLOSE_WINDOW, [CURRENT_TIME, 2, 0, 0, 0]),
            "minimize" => (a.WM_CHANGE_STATE, [3, 0, 0, 0, 0]), // ICCCM IconicState
            "maximize" | "unmaximize" => {
                (a._NET_WM_STATE, [add, a._NET_WM_STATE_MAXIMIZED_HORZ, a._NET_WM_STATE_MAXIMIZED_VERT, 2, 0])
            }
            "fullscreen" | "unfullscreen" => (a._NET_WM_STATE, [add, a._NET_WM_STATE_FULLSCREEN, 0, 2, 0]),
            _ => bail!("unknown window action {action:?}"),
        };
        let event = ClientMessageEvent::new(32, xid, message, data);
        self.conn
            .send_event(false, self.root(), EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, event)?
            .check()?;
        Ok(w.clone())
    }

    fn clipboard(&self) -> (&str, &[&str]) {
        ("xclip", &["-selection", "clipboard", "-in"])
    }

    fn move_abs(&mut self, x: f64, y: f64) -> Result<()> {
        self.input(MOTION_NOTIFY_EVENT, 0, x.round() as i16, y.round() as i16)
    }

    fn button(&mut self, code: u32, press: bool) -> Result<()> {
        let button = match code {
            0x110 => 1,
            0x111 => 3,
            0x112 => 2,
            0x113 => 8,
            0x114 => 9,
            _ => bail!("unknown pointer button {code}"),
        };
        self.pointer_button(button, press)
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        for (axis, delta, negative, positive) in [(0, dx, 6, 7), (1, dy, 4, 5)] {
            let total = self.scroll_remainder[axis] + delta;
            self.scroll_remainder[axis] = total.fract();
            let button = if total < 0.0 { negative } else { positive };
            for _ in 0..total.abs() as u32 {
                self.pointer_button(button, true)?;
                self.pointer_button(button, false)?;
            }
        }
        Ok(())
    }

    fn key(&mut self, code: u32, press: bool) -> Result<()> {
        // Xorg/Xvnc use the XKB evdev map, where X keycodes are offset by eight.
        self.input(if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, (code + 8).try_into()?, 0, 0)
    }
}

/// The existing HUD child protocol, using a click-through override-redirect window.
pub fn hud() -> Result<()> {
    let (c, screen) = x11rb::connect(None).context("connect HUD to X11 DISPLAY")?;
    let screen = &c.setup().roots[screen];
    let root = screen.root;
    let background = c.alloc_color(screen.default_colormap, 0xd6d6, 0x4040, 0x1414)?.reply()?.pixel;
    let message = b"Agent is in control - please don't touch the mouse or keyboard";
    let (width, height) = (message.len() as u16 * 6 + 24, 32);
    let x = c.get_geometry(root)?.reply()?.width.saturating_sub(width) / 2;
    let (window, pixmap, gc, font) = (c.generate_id()?, c.generate_id()?, c.generate_id()?, c.generate_id()?);
    c.create_pixmap(screen.root_depth, pixmap, root, width, height)?.check()?;
    c.open_font(font, b"fixed")?.check()?;
    c.create_gc(gc, pixmap, &CreateGCAux::new().foreground(background).background(background).font(font))?.check()?;
    c.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width, height }])?.check()?;
    c.change_gc(gc, &ChangeGCAux::new().foreground(screen.white_pixel))?.check()?;
    c.image_text8(pixmap, gc, 12, 20, message)?.check()?;
    c.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        x as i16,
        12,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        NONE,
        &CreateWindowAux::new().background_pixmap(pixmap).override_redirect(1_u32),
    )?
    .check()?;
    shape::rectangles(&c, shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, window, 0, 0, &[])?.check()?;

    c.map_window(window)?.check()?;
    crate::hud::serve(
        || {},
        |visible| {
            if visible {
                c.configure_window(window, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?.check()?;
                c.map_window(window)?.check()?;
            } else {
                c.unmap_window(window)?.check()?;
            }
            Ok(())
        },
    )
}
