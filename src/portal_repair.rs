//! Explicit recovery for a user manager restarted without its graphical environment.
//! Never run automatically: restarting the portal interrupts other screen shares.

use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

type Environment = BTreeMap<String, String>;
const KEYS: &[&str] = &["WAYLAND_DISPLAY", "DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];

fn desktop_environment(bytes: &[u8]) -> Option<Environment> {
    let env: Environment = bytes
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let (key, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
            (KEYS.contains(&key) && !value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect();
    let desktop = env.get("XDG_CURRENT_DESKTOP")?;
    if !desktop.split(':').any(|d| d.eq_ignore_ascii_case("COSMIC")) {
        return None;
    }
    Some(env)
}

fn select(mut candidates: Vec<Environment>, display: Option<&str>) -> Result<Environment> {
    if let Some(display) = display {
        candidates.retain(|env| env.get("WAYLAND_DISPLAY").is_some_and(|v| v == display));
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() != 1 {
        bail!(
            "expected one live COSMIC session, found {}; set WAYLAND_DISPLAY to the intended session and retry",
            candidates.len()
        );
    }
    Ok(candidates.remove(0))
}

fn listening_displays(table: &str, owned: &[String], runtime: &Path) -> Vec<String> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 8
                || fields[3] != "00010000"
                || fields[4] != "0001"
                || !owned.iter().any(|inode| inode == fields[6])
            {
                return None;
            }
            let path = Path::new(fields[7]);
            let name = path.file_name()?.to_str()?;
            (path.parent() == Some(runtime) && name.starts_with("wayland-")).then(|| name.to_owned())
        })
        .collect()
}

pub(crate) fn discover(uid: u32, runtime: &Path) -> Result<Environment> {
    let sockets = std::fs::read_to_string("/proc/net/unix")?;
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if !path.metadata().is_ok_and(|m| m.uid() == uid)
            || std::fs::read_to_string(path.join("comm")).unwrap_or_default().trim() != "cosmic-comp"
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path.join("environ")) else {
            continue;
        };
        let Some(env) = desktop_environment(&bytes) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(path.join("fd")) else {
            continue;
        };
        let owned: Vec<String> = fds
            .flatten()
            .filter_map(|fd| {
                let link = std::fs::read_link(fd.path()).ok()?;
                let link = link.to_str()?;
                Some(link.strip_prefix("socket:[")?.strip_suffix(']')?.to_owned())
            })
            .collect();
        // A nested compositor inherits its parent's display, not the socket it serves.
        for display in listening_displays(&sockets, &owned, runtime) {
            let socket = runtime.join(&display);
            if socket
                .metadata()
                .is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid)
            {
                let mut env = env.clone();
                env.insert("WAYLAND_DISPLAY".into(), display);
                candidates.push(env);
            }
        }
    }
    select(candidates, std::env::var("WAYLAND_DISPLAY").ok().as_deref())
}

fn bounded(command: &mut Command, budget: Duration) -> Result<()> {
    let name = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {name}"))?;
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("{name} failed: {status}");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{name} timed out after {}s; inspect portal service status before retrying",
                budget.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn run() -> Result<()> {
    let _control = crate::control_lock::ControlLock::acquire()?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    let runtime = format!("/run/user/{uid}");
    let mut env = discover(uid, Path::new(&runtime))?;
    env.insert("XDG_RUNTIME_DIR".into(), runtime.clone());
    env.insert("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={runtime}/bus"));
    eprintln!(
        "computer-use: restoring {} portal environment for {}",
        env["XDG_CURRENT_DESKTOP"], env["WAYLAND_DISPLAY"]
    );
    bounded(
        Command::new("dbus-update-activation-environment")
            .envs(&env)
            .arg("--systemd")
            .args(env.iter().map(|(key, value)| format!("{key}={value}"))),
        Duration::from_secs(10),
    )?;
    // Stop the frontend first so no request races the backend restart.
    for args in [
        vec!["--user", "stop", "xdg-desktop-portal.service"],
        vec![
            "--user",
            "restart",
            "org.freedesktop.impl.portal.desktop.cosmic.service",
        ],
        vec!["--user", "start", "xdg-desktop-portal.service"],
    ] {
        bounded(Command::new("systemctl").envs(&env).args(args), Duration::from_secs(15))?;
    }
    eprintln!("computer-use: portal services restarted; retry the screenshot");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosmic(display: &str) -> Environment {
        desktop_environment(format!("WAYLAND_DISPLAY={display}\0XDG_CURRENT_DESKTOP=COSMIC\0").as_bytes()).unwrap()
    }

    #[test]
    fn only_graphical_environment_is_exported() {
        let env = desktop_environment(b"WAYLAND_DISPLAY=wayland-1\0XDG_CURRENT_DESKTOP=COSMIC\0DISPLAY=:0\0SECRET=private\0DBUS_SESSION_BUS_ADDRESS=wrong\0").unwrap();
        assert_eq!(env.len(), 3);
        assert_eq!(env["DISPLAY"], ":0");
        assert!(desktop_environment(b"WAYLAND_DISPLAY=wayland-1\0XDG_CURRENT_DESKTOP=GNOME\0").is_none());
        assert!(desktop_environment(b"XDG_CURRENT_DESKTOP=COSMIC\0").is_some());
    }

    #[test]
    fn refuses_ambiguous_or_missing_sessions() {
        let candidates = vec![cosmic("wayland-1"), cosmic("wayland-2")];
        assert!(select(candidates.clone(), None).is_err());
        assert!(select(candidates.clone(), Some("wayland-3")).is_err());
        assert_eq!(
            select(candidates, Some("wayland-2")).unwrap()["WAYLAND_DISPLAY"],
            "wayland-2"
        );
        assert!(select(vec![], None).is_err());
        assert_eq!(
            select(vec![cosmic("wayland-1"), cosmic("wayland-1")], None).unwrap()["WAYLAND_DISPLAY"],
            "wayland-1"
        );
    }

    #[test]
    fn nested_compositor_uses_owned_listener_not_parent_display() {
        let table = "Num RefCount Protocol Flags Type St Inode Path\n\
            000: 00000002 00000000 00010000 0001 01 100 /run/user/1000/wayland-1\n\
            001: 00000002 00000000 00010000 0001 01 200 /run/user/1000/wayland-2\n\
            002: 00000002 00000000 00000000 0001 03 201 /run/user/1000/wayland-1\n";
        assert_eq!(
            listening_displays(table, &["200".into(), "201".into()], Path::new("/run/user/1000")),
            vec!["wayland-2"]
        );
        assert!(listening_displays(table, &["200".into()], Path::new("/run/user/1001")).is_empty());
    }

    #[test]
    fn repair_commands_fail_and_time_out_without_hanging() {
        assert!(bounded(Command::new("sh").args(["-c", "exit 7"]), Duration::from_secs(1)).is_err());
        let start = Instant::now();
        assert!(
            bounded(Command::new("sleep").arg("10"), Duration::from_millis(50))
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
