//! xdg-desktop-portal: RemoteDesktop session (keyboard+pointer, all monitors) and screenshots.
//!
//! Called from blocking threads; async portal calls run on the ambient tokio runtime.
//! Every portal wait is bounded: a portal that never answers (or a consent dialog nobody
//! approves) must surface as an error, because callers hold the desktop mutex while waiting.

use std::future::Future;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::{PersistMode, Session};
use tokio::runtime::Handle;
use tokio::time::timeout;

/// Budget for a negotiation that presents a saved restore token (no dialog expected).
pub(crate) const TOKEN_ATTEMPT: Duration = Duration::from_secs(10);
/// Budget for a negotiation without a token (the user may have to approve a dialog).
pub(crate) const FRESH_ATTEMPT: Duration = Duration::from_secs(25);
const EIS_TIMEOUT: Duration = Duration::from_secs(5);
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

fn token_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("computer-use/restore_token")
}

pub struct RemoteSession {
    inner: Option<(RemoteDesktop, Session<RemoteDesktop>)>,
}

#[derive(Default)]
struct Parts {
    rd: Option<RemoteDesktop>,
    session: Option<Session<RemoteDesktop>>,
}

/// One negotiation with the portal; fills `parts` as it goes so a caller that gives up can close the session.
async fn negotiate(token: Option<&str>, parts: &mut Parts) -> Result<Option<String>> {
    let Parts { rd, session } = parts;
    let rd = &*rd.insert(RemoteDesktop::new().await?);
    let sc = Screencast::new().await?;
    let session = &*session.insert(rd.create_session(Default::default()).await?);
    rd.select_devices(
        session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token),
    )
    .await?
    .response()
    .context("remote desktop device selection was refused")?;
    sc.select_sources(
        session,
        SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
            .set_multiple(true),
    )
    .await?
    .response()
    .context("screen source selection was refused")?;
    let started = rd
        .start(session, None, StartOptions::default())
        .await?
        .response()
        .context("remote desktop session was not approved")?;
    Ok(started.restore_token().map(|t| t.to_owned()))
}

/// A negotiation that cannot outlive `budget`; on failure or timeout the half-made session is closed.
async fn attempt(token: Option<String>, budget: Duration) -> Result<(RemoteDesktop, Session<RemoteDesktop>, Option<String>)> {
    let mut parts = Parts::default();
    let outcome = timeout(budget, negotiate(token.as_deref(), &mut parts)).await;
    match outcome {
        Ok(Ok(new_token)) => {
            let rd = parts.rd.take().context("portal session missing")?;
            let session = parts.session.take().context("portal session missing")?;
            Ok((rd, session, new_token))
        }
        failed => {
            if let Some(session) = parts.session.take() {
                let _ = timeout(CLOSE_TIMEOUT, session.close()).await;
            }
            match failed {
                Ok(Err(e)) => Err(e),
                _ => Err(anyhow!(
                    "timed out after {}s waiting for the desktop portal; a permission dialog may be waiting \
                     for approval, or xdg-desktop-portal is unresponsive",
                    budget.as_secs()
                )),
            }
        }
    }
}

fn save_token(path: &Path, token: &str) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, token)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        eprintln!("computer-use: could not save restore token: {e}");
    }
}

/// Try the saved restore token first; if that fails or times out, discard it and negotiate
/// from scratch. `once(token, budget)` must itself honour `budget`.
pub(crate) async fn start_with_fallback<T, F, Fut>(
    token_path: &Path,
    budgets: (Duration, Duration),
    mut once: F,
) -> Result<T>
where
    F: FnMut(Option<String>, Duration) -> Fut,
    Fut: Future<Output = Result<(T, Option<String>)>>,
{
    let saved = std::fs::read_to_string(token_path).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if saved.is_some() {
        match once(saved, budgets.0).await {
            Ok((v, new)) => {
                if let Some(t) = new {
                    save_token(token_path, &t);
                }
                return Ok(v);
            }
            Err(e) => {
                eprintln!("computer-use: saved restore token failed ({e:#}); discarding it");
                let _ = std::fs::remove_file(token_path);
            }
        }
    }
    let (v, new) = once(None, budgets.1).await.context("could not start a remote desktop session")?;
    if let Some(t) = new {
        save_token(token_path, &t);
    }
    Ok(v)
}

impl RemoteSession {
    /// Shows the approval dialog only when there is no valid saved restore token. Bounded in time.
    pub fn start() -> Result<Self> {
        Handle::current().block_on(async {
            let (rd, session) =
                start_with_fallback(&token_file(), (TOKEN_ATTEMPT, FRESH_ATTEMPT), |token, budget| async move {
                    let (rd, session, new) = attempt(token, budget).await?;
                    Ok(((rd, session), new))
                })
                .await?;
            Ok(Self { inner: Some((rd, session)) })
        })
    }

    pub fn connect_to_eis(&self) -> Result<OwnedFd> {
        let (rd, session) = self.inner.as_ref().ok_or_else(|| anyhow!("session closed"))?;
        Handle::current().block_on(async {
            timeout(EIS_TIMEOUT, rd.connect_to_eis(session, ConnectToEISOptions::default()))
                .await
                .map_err(|_| anyhow!("timed out connecting to the input emulation service"))?
                .map_err(Into::into)
        })
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        if let Some((rd, session)) = self.inner.take() {
            Handle::current().spawn(async move {
                let _ = timeout(CLOSE_TIMEOUT, session.close()).await;
                drop(rd);
            });
        }
    }
}

/// Non-interactive full-desktop screenshot; returns the file the portal wrote.
pub fn screenshot() -> Result<PathBuf> {
    Handle::current().block_on(async {
        let shot = timeout(SCREENSHOT_TIMEOUT, async {
            Screenshot::request().interactive(false).modal(false).send().await?.response()
        })
        .await
        .map_err(|_| anyhow!(
            "timed out waiting for the screenshot portal; restarting the MCP does not restart \
             xdg-desktop-portal. Check `systemctl --user status xdg-desktop-portal.service`. \
             On COSMIC, release desktop control and run `computer-use --repair-portal` to \
             restore the desktop activation environment and restart the portal services \
             (this interrupts existing screen-sharing sessions)"
        ))??;
        url::Url::parse(shot.uri().as_str())?
            .to_file_path()
            .map_err(|_| anyhow!("screenshot uri is not a local file: {}", shot.uri().as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn dir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "cu-portal-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const B: (Duration, Duration) = (Duration::from_millis(50), Duration::from_millis(200));

    #[tokio::test]
    async fn stale_token_is_dropped_and_fresh_attempt_runs() {
        let d = dir();
        let path = d.join("restore_token");
        std::fs::write(&path, "stale\n").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let started = Instant::now();
        let got = start_with_fallback(&path, B, move |token, budget| {
            s2.lock().unwrap().push(token.clone());
            async move {
                match token {
                    // a stale token makes the portal wait on a dialog nobody answers
                    Some(_) => {
                        let r = timeout(budget, std::future::pending::<()>()).await;
                        Err(anyhow!("timed out: {}", r.is_err()))
                    }
                    None => Ok((7, Some("fresh".to_owned()))),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(got, 7);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(*seen.lock().unwrap(), vec![Some("stale".to_owned()), None]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn both_attempts_failing_is_an_error_and_clears_the_token() {
        let d = dir();
        let path = d.join("restore_token");
        std::fs::write(&path, "stale").unwrap();
        let err = start_with_fallback::<(), _, _>(&path, B, |_, _| async { Err(anyhow!("denied")) })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("denied"));
        assert!(!path.exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn no_token_means_one_attempt() {
        let d = dir();
        let path = d.join("restore_token");
        let n = Arc::new(Mutex::new(0));
        let n2 = n.clone();
        start_with_fallback(&path, B, move |t, _| {
            *n2.lock().unwrap() += 1;
            assert!(t.is_none());
            async { Ok(((), None)) }
        })
        .await
        .unwrap();
        assert_eq!(*n.lock().unwrap(), 1);
        std::fs::remove_dir_all(d).unwrap();
    }
}
