//! Server-side desktop state: who is in control, the HUD, the last screenshot's
//! coordinate mapping, and the worker process that does the actual desktop I/O.
//! Nothing here calls into the desktop directly, so nothing here can hang: the HUD
//! waits are bounded (hud.rs) and every worker call has a deadline (worker.rs).

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::control_lock::ControlLock;
use crate::desktop::{PasteKey, Shot};
use crate::ei::Region;
use crate::hud::Hud;
use crate::worker::{self, Backend, Req};

pub struct Controller {
    backend: Backend,
    pub last_shot: Option<Shot>,
    hud: Option<Hud>,
    /// Kept after the HUD so it is dropped last, after the banner is removed.
    control_lock: Option<ControlLock>,
}

impl Controller {
    pub fn new() -> Self {
        Self::with_backend(Backend::new())
    }

    pub fn with_backend(backend: Backend) -> Self {
        Self { backend, last_shot: None, hud: None, control_lock: None }
    }

    /// Take or release control. Taking it shows the HUD; the error string, if any,
    /// reports a HUD failure without refusing control.
    pub fn set_control(&mut self, on: bool) -> Result<Option<String>> {
        if !on {
            self.backend.stop();
            self.last_shot = None;
            self.hud = None;
            self.control_lock = None;
            return Ok(None);
        }
        if self.control_lock.is_none() {
            self.control_lock = Some(ControlLock::acquire()?);
        }
        if self.hud.is_some() {
            return Ok(None);
        }
        match Hud::spawn() {
            Ok(h) => {
                self.hud = Some(h);
                Ok(None)
            }
            Err(e) => Ok(Some(format!("{e:#}"))),
        }
    }

    pub fn require_control(&self) -> Result<()> {
        if self.control_lock.is_none() {
            bail!(
                "the agent is not in control of the desktop; call `control` with enabled=true first \
                 (it shows the user an \"Agent is in control\" HUD so they stop using the mouse and \
                 keyboard), and enabled=false when done"
            );
        }
        Ok(())
    }

    pub fn resolve(&self, x: f64, y: f64, desktop: bool) -> Result<(f64, f64)> {
        if desktop {
            return Ok((x, y));
        }
        let shot = self.last_shot.ok_or_else(|| anyhow!("no screenshot taken yet; take one or use space=desktop"))?;
        Ok(shot.to_desktop(x, y))
    }

    pub fn monitors(&mut self) -> Result<Vec<Region>> {
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
        }
        let reply = self.backend.call(Req::Screenshot { monitor: monitor.map(str::to_owned), region, max_w, max_h });
        if self.hud.as_mut().is_some_and(|h| h.set(true).is_err()) {
            self.hud = None;
        }
        let (png, shot) = worker::screenshot_reply(reply?)?;
        self.last_shot = Some(shot);
        Ok((png, shot))
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.backend.call(Req::Move { x, y }).map(drop)
    }

    pub fn click(&mut self, x: f64, y: f64, button: &str, count: u32, modifiers: &str) -> Result<()> {
        self.backend
            .call(Req::Click { x, y, button: button.into(), count, modifiers: modifiers.into() })
            .map(drop)
    }

    pub fn drag(&mut self, from: (f64, f64), to: (f64, f64), button: &str) -> Result<()> {
        self.backend.call(Req::Drag { from, to, button: button.into() }).map(drop)
    }

    pub fn scroll(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        self.backend.call(Req::Scroll { x, y, dx, dy }).map(drop)
    }

    pub fn key(&mut self, combo: &str, repeat: u32) -> Result<()> {
        self.backend.call(Req::Key { combo: combo.into(), repeat }).map(drop)
    }

    pub fn type_text(&mut self, text: &str, paste: PasteKey) -> Result<String> {
        let v = self.backend.call(Req::Type { text: text.into(), paste })?;
        Ok(v.as_str().unwrap_or("typed").to_owned())
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
        owner.last_shot = Some(Shot { origin_x: 0.0, origin_y: 0.0, scale: 1.0, width: 1, height: 1 });
        owner.set_control(false).unwrap();
        assert!(owner.require_control().is_err());
        assert!(owner.last_shot.is_none());
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
}
