//! Direct Wayland reads that need no portal and no consent dialog: the monitor layout
//! (xdg-output) and screen capture (ext-image-copy-capture). These used to go through
//! xdg-desktop-portal (RemoteDesktop for the layout, Screenshot for pixels), so a stuck
//! portal or an unanswered permission dialog also broke `screenshot`, `monitors` and
//! `windows`. The portal is now only needed to send input.
//!
//! Every wait here has a deadline (`pump`), and the worker process that runs this can
//! be killed besides, so a compositor that never answers costs one call.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use image::{RgbImage, imageops};
use serde::{Deserialize, Serialize};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as source, ext_output_image_capture_source_manager_v1 as source_mgr,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as copy_mgr,
    ext_image_copy_capture_session_v1 as session,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1 as xdg_mgr, zxdg_output_v1 as xdg_out};

use crate::ei::poll_readable;

/// How long a layout query may take.
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one output's capture may take.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

/// A monitor in logical desktop coordinates (the space pointer input and window
/// geometry use). `scale` is physical pixels per logical pixel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub scale: f64,
}

#[derive(Default)]
struct Out {
    wl: Option<wl_output::WlOutput>,
    name: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Current mode in physical pixels (may be rotated relative to the logical size).
    mode: (i32, i32),
    int_scale: i32,
}

#[derive(Default)]
struct State {
    outs: Vec<Out>,
    shm: Option<wl_shm::WlShm>,
    xdg: Option<xdg_mgr::ZxdgOutputManagerV1>,
    sources: Option<source_mgr::ExtOutputImageCaptureSourceManagerV1>,
    copy: Option<copy_mgr::ExtImageCopyCaptureManagerV1>,
    cap: Cap,
}

/// Progress of the one capture in flight.
#[derive(Default)]
struct Cap {
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    constraints_done: bool,
    stopped: bool,
    transform: Option<wl_output::Transform>,
    ready: bool,
    failed: Option<String>,
}

pub struct Display {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
}

impl Display {
    /// Connect and learn the monitor layout.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        let deadline = Instant::now() + LAYOUT_TIMEOUT;
        roundtrip(&conn, &mut queue, &mut state, deadline)?; // globals
        let xdg = state.xdg.clone().ok_or_else(|| anyhow!("compositor lacks zxdg_output_manager_v1"))?;
        for (i, o) in state.outs.iter().enumerate() {
            if let Some(wl) = &o.wl {
                xdg.get_xdg_output(wl, &qh, i);
            }
        }
        // The compositor answers get_xdg_output before this sync: afterwards every output's
        // logical geometry is known. (xdg-output v3 no longer sends its own `done`.)
        roundtrip(&conn, &mut queue, &mut state, deadline).context("monitor layout")?;
        Ok(Self { conn, queue, state })
    }

    /// Monitors left to right, then top to bottom.
    pub fn monitors(&self) -> Vec<Monitor> {
        let mut v: Vec<Monitor> = self
            .state
            .outs
            .iter()
            .filter(|o| o.w > 0 && o.h > 0)
            .map(|o| Monitor {
                name: o.name.clone(),
                x: o.x,
                y: o.y,
                w: o.w,
                h: o.h,
                scale: if o.mode.0 > 0 {
                    o.mode.0.max(o.mode.1) as f64 / o.w.max(o.h) as f64
                } else {
                    o.int_scale.max(1) as f64
                },
            })
            .collect();
        v.sort_by_key(|m| (m.x, m.y));
        v
    }

    /// The whole desktop at logical resolution, and the logical coordinate of its
    /// top-left pixel (the bounding box of all monitors).
    pub fn capture_desktop(&mut self) -> Result<(RgbImage, (i32, i32))> {
        let mons = self.monitors();
        if mons.is_empty() {
            bail!("no monitors");
        }
        let x0 = mons.iter().map(|m| m.x).min().unwrap();
        let y0 = mons.iter().map(|m| m.y).min().unwrap();
        let x1 = mons.iter().map(|m| m.x + m.w).max().unwrap();
        let y1 = mons.iter().map(|m| m.y + m.h).max().unwrap();
        let mut canvas = RgbImage::new((x1 - x0) as u32, (y1 - y0) as u32);
        for m in &mons {
            let img = self.capture_output(&m.name)?;
            let img = if img.dimensions() != (m.w as u32, m.h as u32) {
                imageops::resize(&img, m.w as u32, m.h as u32, imageops::FilterType::Triangle)
            } else {
                img
            };
            imageops::replace(&mut canvas, &img, (m.x - x0) as i64, (m.y - y0) as i64);
        }
        Ok((canvas, (x0, y0)))
    }

    /// One output, upright, at its buffer (physical) resolution, cursor included.
    fn capture_output(&mut self, name: &str) -> Result<RgbImage> {
        let qh = self.queue.handle();
        let out = self
            .state
            .outs
            .iter()
            .find(|o| o.name == name)
            .and_then(|o| o.wl.clone())
            .ok_or_else(|| anyhow!("output {name:?} disappeared"))?;
        let sources = self.state.sources.clone().ok_or_else(|| anyhow!("compositor lacks ext_output_image_capture_source_manager_v1"))?;
        let copy = self.state.copy.clone().ok_or_else(|| anyhow!("compositor lacks ext_image_copy_capture_manager_v1"))?;
        let shm = self.state.shm.clone().ok_or_else(|| anyhow!("compositor lacks wl_shm"))?;
        let deadline = Instant::now() + CAPTURE_TIMEOUT;

        self.state.cap = Cap::default();
        let src = sources.create_source(&out, &qh, ());
        let sess = copy.create_session(&src, copy_mgr::Options::PaintCursors, &qh, ());
        let result = (|| -> Result<RgbImage> {
            pump(&self.conn, &mut self.queue, &mut self.state, deadline, |s| s.cap.constraints_done || s.cap.stopped)
                .context("capture buffer constraints")?;
            if self.state.cap.stopped {
                bail!("compositor stopped the capture session");
            }
            let (w, h) = self.state.cap.size.ok_or_else(|| anyhow!("no capture buffer size"))?;
            let fmt = [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888, wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888]
                .into_iter()
                .find(|f| self.state.cap.formats.contains(f))
                .ok_or_else(|| anyhow!("no supported shm format among {:?}", self.state.cap.formats))?;
            let stride = w * 4;
            let len = (stride * h) as usize;
            let file = shm_file(len)?;
            let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
            let buf = pool.create_buffer(0, w as i32, h as i32, stride as i32, fmt, &qh, ());
            let fr = sess.create_frame(&qh, ());
            fr.attach_buffer(&buf);
            fr.damage_buffer(0, 0, w as i32, h as i32);
            fr.capture();
            let waited = pump(&self.conn, &mut self.queue, &mut self.state, deadline, |s| {
                s.cap.ready || s.cap.failed.is_some()
            });
            fr.destroy();
            buf.destroy();
            pool.destroy();
            waited.context("capture frame")?;
            if let Some(why) = self.state.cap.failed.take() {
                bail!("capture failed: {why}");
            }
            let mut raw = vec![0u8; len];
            file.read_exact_at(&mut raw, 0)?;
            let bgr = matches!(fmt, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
            let img = RgbImage::from_fn(w, h, |x, y| {
                let i = (y * stride + x * 4) as usize;
                // Little-endian XRGB8888 is B,G,R,X in memory; XBGR8888 is R,G,B,X.
                if bgr { image::Rgb([raw[i + 2], raw[i + 1], raw[i]]) } else { image::Rgb([raw[i], raw[i + 1], raw[i + 2]]) }
            });
            Ok(upright(img, self.state.cap.transform.unwrap_or(wl_output::Transform::Normal)))
        })();
        sess.destroy();
        src.destroy();
        let _ = self.conn.flush();
        result
    }
}

/// Undo the output transform so the image reads the way the user sees the screen.
fn upright(img: RgbImage, t: wl_output::Transform) -> RgbImage {
    use wl_output::Transform as T;
    // wl_output transforms rotate counter-clockwise; image's rotateN turn clockwise.
    let flip = |i: RgbImage| imageops::flip_horizontal(&i);
    match t {
        T::_90 => imageops::rotate270(&img),
        T::_180 => imageops::rotate180(&img),
        T::_270 => imageops::rotate90(&img),
        T::Flipped => flip(img),
        T::Flipped90 => imageops::rotate270(&flip(img)),
        T::Flipped180 => imageops::rotate180(&flip(img)),
        T::Flipped270 => imageops::rotate90(&flip(img)),
        _ => img,
    }
}

/// An anonymous shared-memory file for the capture buffer (unlinked right away).
fn shm_file(len: usize) -> Result<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = dir.join(format!(
        "computer-use-shm-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = File::options().read(true).write(true).create_new(true).open(&path).context("create capture buffer")?;
    let _ = std::fs::remove_file(&path);
    file.set_len(len as u64)?;
    Ok(file)
}

fn roundtrip(conn: &Connection, q: &mut EventQueue<State>, s: &mut State, deadline: Instant) -> Result<()> {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    conn.display().sync(&q.handle(), done.clone());
    pump(conn, q, s, deadline, |_| done.load(std::sync::atomic::Ordering::SeqCst))
}

/// Dispatch events until `done` holds, never past `deadline`.
fn pump(
    conn: &Connection,
    q: &mut EventQueue<State>,
    s: &mut State,
    deadline: Instant,
    done: impl Fn(&State) -> bool,
) -> Result<()> {
    loop {
        q.dispatch_pending(s)?;
        if done(s) {
            return Ok(());
        }
        conn.flush()?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("the compositor did not answer within the deadline");
        }
        if let Some(guard) = q.prepare_read() {
            use std::os::fd::AsRawFd;
            match poll_readable(guard.connection_fd().as_raw_fd(), left.min(Duration::from_millis(100))) {
                Some(true) => match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                },
                Some(false) => drop(guard),
                None => bail!("the Wayland connection was closed"),
            }
        }
    }
}

// ---- event plumbing ----------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_output" => {
                    let i = s.outs.len();
                    s.outs.push(Out { wl: Some(reg.bind(name, version.min(4), qh, i)), int_scale: 1, ..Default::default() });
                }
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "zxdg_output_manager_v1" => s.xdg = Some(reg.bind(name, version.min(3), qh, ())),
                "ext_output_image_capture_source_manager_v1" => s.sources = Some(reg.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => s.copy = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, std::sync::Arc<std::sync::atomic::AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        done: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        done.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(s: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = s.outs.get_mut(*i) else { return };
        match ev {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Scale { factor } => o.int_scale = factor,
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. } if f.contains(wl_output::Mode::Current) => {
                o.mode = (width, height);
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_mgr::ZxdgOutputManagerV1, ()> for State {
    fn event(_: &mut Self, _: &xdg_mgr::ZxdgOutputManagerV1, _: xdg_mgr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<xdg_out::ZxdgOutputV1, usize> for State {
    fn event(s: &mut Self, _: &xdg_out::ZxdgOutputV1, ev: xdg_out::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = s.outs.get_mut(*i) else { return };
        match ev {
            xdg_out::Event::LogicalPosition { x, y } => (o.x, o.y) = (x, y),
            xdg_out::Event::LogicalSize { width, height } => (o.w, o.h) = (width, height),
            xdg_out::Event::Name { name } if o.name.is_empty() => o.name = name,
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<source_mgr::ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn event(_: &mut Self, _: &source_mgr::ExtOutputImageCaptureSourceManagerV1, _: source_mgr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<source::ExtImageCaptureSourceV1, ()> for State {
    fn event(_: &mut Self, _: &source::ExtImageCaptureSourceV1, _: source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<copy_mgr::ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(_: &mut Self, _: &copy_mgr::ExtImageCopyCaptureManagerV1, _: copy_mgr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(s: &mut Self, _: &session::ExtImageCopyCaptureSessionV1, ev: session::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            session::Event::BufferSize { width, height } => s.cap.size = Some((width, height)),
            session::Event::ShmFormat { format: WEnum::Value(f) } => s.cap.formats.push(f),
            session::Event::Done => s.cap.constraints_done = true,
            session::Event::Stopped => s.cap.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(s: &mut Self, _: &frame::ExtImageCopyCaptureFrameV1, ev: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            frame::Event::Transform { transform: WEnum::Value(t) } => s.cap.transform = Some(t),
            frame::Event::Ready => s.cap.ready = true,
            frame::Event::Failed { reason } => {
                s.cap.failed = Some(match reason {
                    WEnum::Value(frame::FailureReason::BufferConstraints) => "buffer constraints changed".into(),
                    WEnum::Value(frame::FailureReason::Stopped) => "session stopped".into(),
                    other => format!("{other:?}"),
                });
            }
            _ => {}
        }
    }
}
