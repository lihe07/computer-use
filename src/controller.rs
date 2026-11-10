//! Server-side desktop state: who is in control, the HUD, the last screenshot's
//! coordinate mapping, and the worker process that does the actual desktop I/O.
//! Nothing here calls into the desktop directly, so nothing here can hang: the HUD
//! waits are bounded (hud.rs) and every worker call has a deadline (worker.rs).

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::control_lock::ControlLock;
use crate::desktop::{PasteKey, Shot};
use crate::hud::Hud;
use crate::wl::Monitor;
use crate::worker::{self, Backend, Req};

/// An owner that makes no desktop call for this long loses control, so a session that
/// forgot to release (or died in a way that kept its server alive) does not lock the
/// user and every other agent out for hours.
pub const IDLE_RELEASE: Duration = Duration::from_secs(5 * 60);
/// After control is released the worker (and its portal session, so no new consent
/// dialog) is kept this long for the next `control`, then stopped.
const WORKER_LINGER: Duration = Duration::from_secs(10 * 60);
/// How often a missing HUD is retried while in control.
const HUD_RETRY: Duration = Duration::from_secs(30);

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub struct Controller {
    backend: Backend,
    last_shot: Option<(Shot, Instant)>,
    hud: Option<Hud>,
    /// Why the HUD is not up while in control, and when that was last tried.
    hud_error: Option<(String, Instant)>,
    /// Kept after the HUD so it is dropped last, after the banner is removed.
    control_lock: Option<ControlLock>,
    last_activity: Instant,
    /// Set when control was taken away for inactivity; reported on the next call.
    auto_released: Option<Instant>,
    idle_release: Duration,
}

impl Controller {
    pub fn new() -> Self {
        Self::with_backend(Backend::new())
    }

    pub fn with_backend(backend: Backend) -> Self {
        Self {
            backend,
            last_shot: None,
            hud: None,
            hud_error: None,
            control_lock: None,
            last_activity: Instant::now(),
            auto_released: None,
            idle_release: IDLE_RELEASE,
        }
    }

    #[cfg(test)]
    pub(crate) fn hold_for_test(&mut self, lock: ControlLock) {
        self.control_lock = Some(lock);
    }

    pub fn in_control(&self) -> bool {
        self.control_lock.is_some()
    }

    /// Record a tool call (keeps control from timing out).
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Take or release control. Taking it shows the HUD; the error string, if any,
    /// reports a HUD failure without refusing control.
    pub fn set_control(&mut self, on: bool) -> Result<Option<String>> {
        self.auto_released = None;
        if !on {
            self.release();
            return Ok(None);
        }
        if self.control_lock.is_none() {
            self.control_lock = Some(ControlLock::acquire()?);
        }
        if self.hud.is_some() {
            return Ok(None);
        }
        self.hud_error = None;
        // One retry when taking control: a HUD that fails once (compositor busy) usually works next.
        Ok(self.spawn_hud().and_then(|_| self.spawn_hud()))
    }

    /// Give up control. The worker keeps its portal session (see WORKER_LINGER) and the
    /// screenshot mapping stays valid: neither can act without control.
    fn release(&mut self) {
        self.hud = None;
        self.hud_error = None;
        self.control_lock = None;
    }

    /// Try once to show the HUD; the error text if it could not be shown.
    fn spawn_hud(&mut self) -> Option<String> {
        match Hud::spawn() {
            Ok(h) => {
                self.hud = Some(h);
                self.hud_error = None;
                None
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.hud_error = Some((msg.clone(), Instant::now()));
                Some(msg)
            }
        }
    }

    /// Housekeeping, called periodically by the server: idle release and worker reaping.
    pub fn tick(&mut self) {
        let idle = self.last_activity.elapsed();
        if self.in_control() && idle >= self.idle_release {
            eprintln!("computer-use: releasing control after {} without desktop calls", human(idle));
            self.release();
            self.auto_released = Some(Instant::now());
        }
        if !self.in_control() && idle >= WORKER_LINGER && self.backend.is_running() {
            self.backend.stop();
        }
    }

    pub fn require_control(&mut self) -> Result<()> {
        if self.control_lock.is_none() {
            let why = match self.auto_released {
                Some(at) => format!(
                    "control was released automatically {} ago after {} without desktop calls; ",
                    human(at.elapsed()),
                    human(self.idle_release)
                ),
                None => String::new(),
            };
            bail!(
                "the agent is not in control of the desktop; {why}call `control` with enabled=true first \
                 (it shows the user an \"Agent is in control\" HUD so they stop using the mouse and \
                 keyboard), and enabled=false when done"
            );
        }
        if self.hud.is_none()
            && self.hud_error.as_ref().is_some_and(|(_, at)| at.elapsed() >= HUD_RETRY)
        {
            self.spawn_hud();
        }
        Ok(())
    }

    /// Warnings to append to a tool result.
    pub fn notes(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some((e, _)) = self.hud_error.as_ref().filter(|_| self.in_control()) {
            v.push(format!(
                "WARNING: the \"Agent is in control\" HUD is not showing ({e}); the user cannot see that \
                 you are using the desktop. Tell them before acting"
            ));
        }
        v
    }

    /// Map a point to desktop coordinates; also says how old the mapping screenshot is.
    pub fn resolve(&self, x: f64, y: f64, desktop: bool) -> Result<((f64, f64), Option<Duration>)> {
        if desktop {
            return Ok(((x, y), None));
        }
        let (shot, at) = self.last_shot.ok_or_else(|| {
            anyhow!("no screenshot taken yet by this server; take one first (space=\"image\" coordinates refer to it), or pass space=\"desktop\"")
        })?;
        if x < 0.0 || y < 0.0 || x > shot.width as f64 || y > shot.height as f64 {
            bail!(
                "({x}, {y}) is outside the last screenshot ({}x{}); image coordinates refer to the most recent screenshot",
                shot.width,
                shot.height
            );
        }
        Ok((shot.to_desktop(x, y), Some(at.elapsed())))
    }

    pub fn monitors(&mut self) -> Result<Vec<Monitor>> {
        Ok(serde_json::from_value(self.backend.call(Req::Monitors)?)?)
    }

    pub fn screenshot(
        &mut self,
        monitor: Option<&str>,
        region: Option<(i64, i64, i64, i64)>,
        max_w: u32,
        max_h: u32,
    ) -> Result<(Vec<u8>, Shot)> {
        // Keep the HUD out of the agent's view; a failed hide/show only costs the HUD.
        if self.hud.as_mut().is_some_and(|h| h.set(false).is_err()) {
            self.hud = None;
            self.hud_error = Some(("it stopped responding".into(), Instant::now()));
        }
        let reply = self.backend.call(Req::Screenshot { monitor: monitor.map(str::to_owned), region, max_w, max_h });
        if self.hud.as_mut().is_some_and(|h| h.set(true).is_err()) {
            self.hud = None;
            self.hud_error = Some(("it stopped responding".into(), Instant::now()));
        }
        let (png, shot) = worker::screenshot_reply(reply?)?;
        self.last_shot = Some((shot, Instant::now()));
        Ok((png, shot))
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<Value> {
        self.backend.call(Req::Move { x, y })
    }

    pub fn click(&mut self, x: f64, y: f64, button: &str, count: u32, modifiers: &str, expect: Option<String>) -> Result<Value> {
        self.backend.call(Req::Click { x, y, button: button.into(), count, modifiers: modifiers.into(), expect })
    }

    pub fn drag(&mut self, from: (f64, f64), to: (f64, f64), button: &str) -> Result<Value> {
        self.backend.call(Req::Drag { from, to, button: button.into() })
    }

    pub fn scroll(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> Result<Value> {
        self.backend.call(Req::Scroll { x, y, dx, dy })
    }

    pub fn key(&mut self, combo: &str, repeat: u32, expect: Option<String>) -> Result<Value> {
        self.backend.call(Req::Key { combo: combo.into(), repeat, expect })
    }

    pub fn type_text(&mut self, text: &str, paste: PasteKey, expect: Option<String>) -> Result<Value> {
        self.backend.call(Req::Type { text: text.into(), paste, expect })
    }

    pub fn clipboard(&mut self, set: Option<String>) -> Result<String> {
        let v = self.backend.call(Req::Clipboard { set })?;
        Ok(v.as_str().unwrap_or_default().to_owned())
    }

    pub fn windows(&mut self) -> Result<Value> {
        self.backend.call(Req::Windows)
    }

    pub fn window_action(&mut self, query: &str, action: &str) -> Result<Value> {
        self.backend.call(Req::Window { query: query.into(), action: action.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_lock::tests::TestPath;

    fn shot() -> Shot {
        Shot { origin_x: 10.0, origin_y: 20.0, scale: 0.5, width: 100, height: 50 }
    }

    #[test]
    fn releasing_non_owner_does_not_unlock_owner() {
        let path = TestPath::new();
        let mut owner = Controller::new();
        owner.control_lock = Some(ControlLock::at(&path.0).unwrap());
        owner.require_control().unwrap();
        let mut other = Controller::new();
        other.set_control(false).unwrap();
        assert!(other.require_control().is_err());
        assert!(ControlLock::at(&path.0).is_err());
        owner.set_control(false).unwrap();
        assert!(owner.require_control().is_err());
        let _next_owner = ControlLock::at(&path.0).unwrap();
    }

    #[test]
    fn dropping_controller_releases_ownership() {
        let path = TestPath::new();
        let mut owner = Controller::new();
        owner.control_lock = Some(ControlLock::at(&path.0).unwrap());
        assert!(ControlLock::at(&path.0).is_err());
        drop(owner);
        let _next_owner = ControlLock::at(&path.0).unwrap();
    }

    #[test]
    fn screenshot_mapping_survives_release() {
        let path = TestPath::new();
        let mut c = Controller::new();
        c.control_lock = Some(ControlLock::at(&path.0).unwrap());
        c.last_shot = Some((shot(), Instant::now()));
        c.set_control(false).unwrap();
        let ((x, y), age) = c.resolve(50.0, 40.0, false).unwrap();
        assert_eq!((x, y), (110.0, 100.0));
        assert!(age.is_some());
    }

    #[test]
    fn points_outside_the_screenshot_are_refused() {
        let mut c = Controller::new();
        c.last_shot = Some((shot(), Instant::now()));
        assert!(c.resolve(101.0, 10.0, false).unwrap_err().to_string().contains("outside"));
        assert!(c.resolve(-1.0, 10.0, false).is_err());
        assert!(c.resolve(5000.0, 5000.0, true).is_ok(), "desktop space is not checked against the image");
    }

    #[test]
    fn idle_owner_loses_control_and_is_told_why() {
        let path = TestPath::new();
        let mut c = Controller::new();
        c.idle_release = Duration::from_millis(1);
        c.control_lock = Some(ControlLock::at(&path.0).unwrap());
        c.last_activity = Instant::now() - Duration::from_secs(1);
        c.tick();
        assert!(!c.in_control());
        let _other = ControlLock::at(&path.0).expect("lock must be free after idle release");
        let err = c.require_control().unwrap_err().to_string();
        assert!(err.contains("released automatically"), "{err}");
        // Taking control again clears the note.
        drop(_other);
        c.control_lock = Some(ControlLock::at(&path.0).unwrap());
        c.auto_released = None;
        c.require_control().unwrap();
    }

    #[test]
    fn active_owner_keeps_control() {
        let path = TestPath::new();
        let mut c = Controller::new();
        c.control_lock = Some(ControlLock::at(&path.0).unwrap());
        c.touch();
        c.tick();
        assert!(c.in_control());
    }

    #[test]
    fn missing_hud_is_warned_about_while_in_control() {
        let path = TestPath::new();
        let mut c = Controller::new();
        c.control_lock = Some(ControlLock::at(&path.0).unwrap());
        c.hud_error = Some(("no compositor".into(), Instant::now()));
        assert!(c.notes().iter().any(|n| n.contains("HUD is not showing")));
        c.set_control(false).unwrap();
        assert!(c.notes().is_empty());
    }
}
