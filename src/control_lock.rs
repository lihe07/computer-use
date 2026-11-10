//! One owner across all MCP server processes for the logged-in desktop user.

use std::fs::{File, TryLockError};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct ControlLock {
    // Closing this descriptor releases the lock on process death.
    file: File,
}

impl Drop for ControlLock {
    fn drop(&mut self) {
        // Explicitly unlock before closing: a concurrently forked child can hold
        // a duplicate descriptor briefly until exec closes it.
        let _ = self.file.unlock();
    }
}

impl ControlLock {
    pub fn acquire() -> Result<Self> {
        // Use a fixed per-user path even if hosts supply different environments.
        let uid = std::fs::metadata("/proc/self").context("determine desktop user")?.uid();
        Self::at(Path::new(&format!("/run/user/{uid}/computer-use.lock")))
    }

    pub(crate) fn at(path: &Path) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open desktop control lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                let mut lock = Self { file };
                lock.write_holder();
                Ok(lock)
            }
            Err(TryLockError::WouldBlock) => {
                let mut held_by = String::new();
                let _ = (&file).read_to_string(&mut held_by);
                bail!(
                    "desktop is busy: another agent holds control{}. Wait until it releases control before \
                     retrying `control` with enabled=true; an owner that makes no desktop call for {} minutes \
                     loses control automatically",
                    describe_holder(&held_by).map(|h| format!(" ({h})")).unwrap_or_default(),
                    crate::controller::IDLE_RELEASE.as_secs() / 60,
                )
            }
            Err(TryLockError::Error(e)) => Err(e).context("lock desktop control"),
        }
        // Never unlink the file: a waiter may already have opened the same inode.
    }
}

impl ControlLock {
    /// Record who holds the lock, for the busy error other agents get. The lock itself
    /// is the flock; this text is only a description and may be stale or empty.
    fn write_holder(&mut self) {
        let pid = std::process::id();
        let session = parent_session();
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let since = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let text = serde_json::json!({ "pid": pid, "session": session, "cwd": cwd, "since": since }).to_string();
        let _ = self.file.set_len(0);
        let _ = self.file.rewind();
        let _ = self.file.write_all(text.as_bytes());
        let _ = self.file.flush();
    }
}

/// The MCP host that started us: its `-n <name>` / `--name <name>` if it has one
/// (Claude Code session names), else its command name.
fn parent_session() -> String {
    let ppid = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| s.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1).map(str::to_owned)))
        .unwrap_or_default();
    let args: Vec<String> = std::fs::read(format!("/proc/{ppid}/cmdline"))
        .map(|b| b.split(|c| *c == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
        .unwrap_or_default();
    let named = args.windows(2).find(|w| w[0] == "-n" || w[0] == "--name").map(|w| w[1].clone());
    named.unwrap_or_else(|| {
        args.first().map(|a| a.rsplit('/').next().unwrap_or(a).to_owned()).unwrap_or_default()
    })
}

fn describe_holder(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let pid = v.get("pid")?.as_u64()?;
    let mut parts = vec![format!("server pid {pid}")];
    if let Some(s) = v.get("session").and_then(|s| s.as_str()).filter(|s| !s.is_empty()) {
        parts.push(format!("session {s:?}"));
    }
    if let Some(c) = v.get("cwd").and_then(|s| s.as_str()).filter(|s| !s.is_empty()) {
        parts.push(format!("cwd {c}"));
    }
    if let Some(since) = v.get("since").and_then(|s| s.as_u64()) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(since);
        parts.push(format!("for {}", crate::controller::human(std::time::Duration::from_secs(now.saturating_sub(since)))));
    }
    Some(parts.join(", "))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    pub struct TestPath(pub PathBuf);

    impl TestPath {
        pub fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "computer-use-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir.join("control.lock"))
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn exclusive_until_released() {
        let path = TestPath::new();
        let owner = ControlLock::at(&path.0).unwrap();
        let busy = ControlLock::at(&path.0).unwrap_err().to_string();
        assert!(busy.contains("another agent"), "{busy}");
        assert!(busy.contains(&format!("server pid {}", std::process::id())), "{busy}");
        drop(owner);
        let _next_owner = ControlLock::at(&path.0).unwrap();
    }

    #[test]
    fn release_is_not_delayed_by_inherited_descriptor() {
        let path = TestPath::new();
        let owner = ControlLock::at(&path.0).unwrap();
        let _inherited = owner.file.try_clone().unwrap();
        drop(owner);
        let _next_owner = ControlLock::at(&path.0).unwrap();
    }

    #[test]
    fn simultaneous_acquisition_has_one_winner() {
        let path = TestPath::new();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let finish = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (start, finish, path) = (start.clone(), finish.clone(), path.0.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let lock = ControlLock::at(&path);
                    finish.wait(); // Keep the winner's lock until all contenders have tried.
                    lock.is_ok()
                })
            })
            .collect();
        let winners = workers.into_iter().map(|w| usize::from(w.join().unwrap())).sum::<usize>();
        assert_eq!(winners, 1);
    }

    // Invoked in a separate process by the test below; no desktop or HUD needed.
    #[test]
    fn child() {
        let Some(path) = std::env::var_os("COMPUTER_USE_TEST_LOCK") else { return };
        let _lock = ControlLock::at(Path::new(&path)).unwrap();
        println!("LOCKED");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn process_death_releases_lock() {
        let path = TestPath::new();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "control_lock::tests::child", "--nocapture"])
                .env("COMPUTER_USE_TEST_LOCK", &path.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap() == "LOCKED" {
                    let _ = tx.send(());
                    break;
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("child must acquire lock");
        assert!(ControlLock::at(&path.0).unwrap_err().to_string().contains("another agent"));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let _next_owner = ControlLock::at(&path.0).unwrap();
    }
}
