//! xdg-desktop-portal: RemoteDesktop session (keyboard+pointer, all monitors) and screenshots.
//!
//! Called from blocking threads; async portal calls run on the ambient tokio runtime.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::{PersistMode, Session};
use tokio::runtime::Handle;

fn token_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("computer-use/restore_token")
}

pub struct RemoteSession {
    inner: Option<(RemoteDesktop, Session<RemoteDesktop>)>,
}

impl RemoteSession {
    /// Shows the approval dialog only the first time; the restore token is persisted.
    pub fn start() -> Result<Self> {
        Handle::current().block_on(async {
            let token_path = token_file();
            let token = std::fs::read_to_string(&token_path).ok().map(|s| s.trim().to_owned());
            let rd = RemoteDesktop::new().await?;
            let sc = Screencast::new().await?;
            let session = rd.create_session(Default::default()).await?;
            rd.select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?
            .response()?;
            sc.select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                    .set_multiple(true),
            )
            .await?
            .response()?;
            let started = rd
                .start(&session, None, StartOptions::default())
                .await?
                .response()
                .context("remote desktop session was not approved")?;
            if let Some(t) = started.restore_token() {
                if let Some(dir) = token_path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&token_path, t)?;
            }
            Ok(Self { inner: Some((rd, session)) })
        })
    }

    pub fn connect_to_eis(&self) -> Result<OwnedFd> {
        let (rd, session) = self.inner.as_ref().ok_or_else(|| anyhow!("session closed"))?;
        Ok(Handle::current().block_on(rd.connect_to_eis(session, ConnectToEISOptions::default()))?)
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        if let Some((rd, session)) = self.inner.take() {
            Handle::current().spawn(async move {
                let _ = session.close().await;
                drop(rd);
            });
        }
    }
}

/// Non-interactive full-desktop screenshot; returns the file the portal wrote.
pub fn screenshot() -> Result<PathBuf> {
    Handle::current().block_on(async {
        let shot = Screenshot::request().interactive(false).modal(false).send().await?.response()?;
        url::Url::parse(shot.uri().as_str())?
            .to_file_path()
            .map_err(|_| anyhow!("screenshot uri is not a local file: {}", shot.uri().as_str()))
    })
}
