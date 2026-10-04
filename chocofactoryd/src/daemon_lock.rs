//! The daemon's side of the lock file (see `chocofactory_core::daemon_lock`
//! for the format and the reader). The file is never deleted.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use chocofactory_core::daemon_lock::{LockInfo, LockState, lock_path, read_lock};

const RETRY_EVERY: Duration = Duration::from_millis(50);
/// A reader's shared lock is momentary, so a refused exclusive lock is
/// retried this long before it is believed.
const RETRY_FOR: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum AcquireError {
    /// Another daemon holds the lock; its published info, if it parses.
    Held {
        root: PathBuf,
        info: Option<LockInfo>,
    },
    Io(io::Error),
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcquireError::Held {
                root,
                info: Some(info),
            } => write!(
                f,
                "another chocofactoryd (pid {}, port {}) is already running for {}",
                info.pid,
                info.port,
                root.display()
            ),
            AcquireError::Held { root, info: None } => write!(
                f,
                "another chocofactoryd is already running for {}",
                root.display()
            ),
            AcquireError::Io(e) => write!(f, "could not take the daemon lock: {e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// Holds the exclusive `flock` for as long as it lives. Dropping it (or the
/// process dying, even by SIGKILL) releases the lock.
pub struct DaemonLock {
    file: File,
}

impl DaemonLock {
    pub fn acquire(root: &Path) -> Result<DaemonLock, AcquireError> {
        // No truncate: a refused attempt must not wipe the live daemon's info.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(root))
            .map_err(AcquireError::Io)?;
        let deadline = Instant::now() + RETRY_FOR;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(DaemonLock { file }),
                Err(TryLockError::Error(e)) => return Err(AcquireError::Io(e)),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        let info = match read_lock(root) {
                            Ok(LockState::Running(info)) => Some(info),
                            _ => None,
                        };
                        return Err(AcquireError::Held {
                            root: root.to_path_buf(),
                            info,
                        });
                    }
                    std::thread::sleep(RETRY_EVERY);
                }
            }
        }
    }

    pub fn publish(&self, info: &LockInfo) -> io::Result<()> {
        let json = serde_json::to_vec(info).map_err(io::Error::other)?;
        let mut file = &self.file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&json)?;
        file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("choco-daemon-lock-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn info() -> LockInfo {
        LockInfo {
            pid: 4242,
            port: 4545,
            version: "0.1.0".into(),
            commit: Some("abc1234".into()),
            started_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            exe: "/x/chocofactoryd".into(),
        }
    }

    #[test]
    fn acquire_then_publish_is_visible_to_read_lock() {
        let t = Tmp::new();
        let lock = DaemonLock::acquire(&t.0).unwrap();
        lock.publish(&info()).unwrap();
        assert_eq!(read_lock(&t.0).unwrap(), LockState::Running(info()));
        // A shorter republish leaves no stale tail.
        let mut short = info();
        short.commit = None;
        lock.publish(&short).unwrap();
        assert_eq!(read_lock(&t.0).unwrap(), LockState::Running(short));
    }

    #[test]
    fn a_second_acquire_is_refused_and_the_file_survives_release() {
        let t = Tmp::new();
        let first = DaemonLock::acquire(&t.0).unwrap();
        first.publish(&info()).unwrap();
        let err = DaemonLock::acquire(&t.0).err().expect("must be held");
        match &err {
            AcquireError::Held { info: Some(i), .. } => assert_eq!(i, &info()),
            other => panic!("unexpected {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("pid 4242") && msg.contains("port 4545"),
            "{msg}"
        );
        // The refused attempt didn't wipe the info.
        assert_eq!(read_lock(&t.0).unwrap(), LockState::Running(info()));

        drop(first);
        assert!(lock_path(&t.0).exists());
        assert_eq!(
            read_lock(&t.0).unwrap(),
            LockState::NotRunning { last: Some(info()) }
        );
        DaemonLock::acquire(&t.0).unwrap();
    }

    #[test]
    fn held_without_parsable_info_names_no_pid() {
        let t = Tmp::new();
        let _first = DaemonLock::acquire(&t.0).unwrap();
        let err = DaemonLock::acquire(&t.0).err().unwrap();
        assert_eq!(
            err.to_string(),
            format!(
                "another chocofactoryd is already running for {}",
                t.0.display()
            )
        );
    }

    #[test]
    fn the_lock_is_not_inherited_by_children() {
        let t = Tmp::new();
        let lock = DaemonLock::acquire(&t.0).unwrap();
        lock.publish(&info()).unwrap();
        // The child blocks reading a stdin pipe this test holds open, so it
        // stays alive until the test says otherwise. A `sleep N` ran out its
        // own clock under load (#152), making `alive` false for a reason
        // unrelated to the lock. `alive` is still asserted, so the test
        // can't pass vacuously with a child that is already gone.
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        drop(lock);
        let state = read_lock(&t.0).unwrap();
        let alive = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        assert!(alive);
        assert!(matches!(state, LockState::NotRunning { .. }), "{state:?}");
    }
}
