//! The daemon's lock file: `<config_root>/chocofactoryd.lock`.
//!
//! The daemon holds a kernel `flock` on it for as long as it runs and
//! writes a [`LockInfo`] JSON document into it. The file is never deleted:
//! unlinking a locked file would let a second daemon lock a fresh inode
//! while the first still runs. The kernel drops the lock when the process
//! dies, so a leftover file is harmless.

use std::fs::{File, TryLockError};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const LOCK_FILE_NAME: &str = "chocofactoryd.lock";

pub fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_FILE_NAME)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub port: u16,
    pub version: String,
    pub commit: Option<String>,
    pub started_at: DateTime<Utc>,
    pub exe: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    /// Nobody holds the lock; `last` is what the previous daemon wrote.
    NotRunning {
        last: Option<LockInfo>,
    },
    Running(LockInfo),
}

fn parse(file: &mut File) -> Option<LockInfo> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

const RETRY_EVERY: Duration = Duration::from_millis(50);
const HELD_BUT_UNREADABLE_FOR: Duration = Duration::from_secs(1);

pub fn read_lock(root: &Path) -> io::Result<LockState> {
    let path = lock_path(root);
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(LockState::NotRunning { last: None });
        }
        Err(e) => return Err(e),
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let last = parse(&mut file);
            file.unlock()?;
            return Ok(LockState::NotRunning { last });
        }
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(e)) => return Err(e),
    }
    // A daemon holds it. It may be between locking and writing, so give
    // the JSON a moment to appear.
    let deadline = Instant::now() + HELD_BUT_UNREADABLE_FOR;
    loop {
        // Re-open: a fresh handle reads from the start of whatever the
        // daemon has written by now.
        let mut reader = File::open(&path)?;
        if let Some(info) = parse(&mut reader) {
            return Ok(LockState::Running(info));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{LOCK_FILE_NAME} is held but unreadable"),
            ));
        }
        std::thread::sleep(RETRY_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir()
                .join(format!("choco-core-lock-{}", std::process::id()))
                .join(format!("{:?}", std::thread::current().id()).replace(['(', ')'], ""));
            let _ = std::fs::remove_dir_all(&p);
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
            pid: 42,
            port: 4141,
            version: "0.1.0".into(),
            commit: None,
            started_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            exe: "/bin/chocofactoryd".into(),
        }
    }

    #[test]
    fn no_file_is_not_running() {
        let t = Tmp::new();
        assert_eq!(
            read_lock(&t.0).unwrap(),
            LockState::NotRunning { last: None }
        );
    }

    #[test]
    fn unlocked_file_reports_last_info() {
        let t = Tmp::new();
        std::fs::write(lock_path(&t.0), serde_json::to_string(&info()).unwrap()).unwrap();
        assert_eq!(
            read_lock(&t.0).unwrap(),
            LockState::NotRunning { last: Some(info()) }
        );
    }

    #[test]
    fn held_lock_reports_running() {
        let t = Tmp::new();
        let mut holder = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(&t.0))
            .unwrap();
        holder.try_lock().unwrap();
        holder
            .write_all(serde_json::to_string(&info()).unwrap().as_bytes())
            .unwrap();
        holder.sync_data().unwrap();
        assert_eq!(read_lock(&t.0).unwrap(), LockState::Running(info()));
    }

    #[test]
    fn held_but_empty_errors_after_about_a_second() {
        let t = Tmp::new();
        let holder = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(&t.0))
            .unwrap();
        holder.try_lock().unwrap();
        let start = Instant::now();
        let err = read_lock(&t.0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("held but unreadable"));
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
