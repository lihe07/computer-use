//! One owner across all MCP server processes for the logged-in desktop user.

use std::fs::{File, TryLockError};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

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
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => bail!(
                "desktop is busy: another agent holds control; wait until it releases control \
                 before retrying `control` with enabled=true"
            ),
            Err(TryLockError::Error(e)) => Err(e).context("lock desktop control"),
        }
        // Never unlink the file: a waiter may already have opened the same inode.
    }
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
        assert!(ControlLock::at(&path.0).unwrap_err().to_string().contains("another agent"));
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
