//! Shared by `server.rs` and `install.rs`: kills everything a test left
//! running in its temporary `HOME`.
//!
//! `choco server start` spawns the daemon with `setsid` and with its current
//! directory set to `$HOME`, and the daemon never changes directory. Each
//! test `Env` has a unique temporary `HOME`, so "every process whose cwd is
//! inside that home" is exactly the daemons (and any fakes or helpers) the
//! test left behind, whatever the lock file says. The lock-based kill in
//! `Env::drop` only finds a daemon that currently holds the lock's flock.
//!
//! Accepted limits:
//! - A `cargo test` killed by a signal (SIGKILL, CI cancel) never runs
//!   `drop`, so its daemons leak. `drop` is the only hook a test has, and a
//!   sweep at the next start could not tell a dead run's home from a
//!   concurrent run's.
//! - A process a test daemon spawned whose cwd is outside the home (say a
//!   fixture `claude` run in a repo elsewhere) is not found. The fixtures
//!   put repos under the home.
//! - A pid reused between listing and kill is possible in theory; the
//!   window is microseconds.

use std::path::{Path, PathBuf};

/// SIGKILLs every process (other than this one) whose current directory is
/// `home` or inside it, and returns the pids it signalled. `home` is
/// canonicalized first (falling back to the path as given if it no longer
/// exists), because the kernel reports canonical paths (`/private/var/…` on
/// macOS). Processes whose cwd can't be read are not ours and are skipped.
pub fn kill_processes_under(home: &Path) -> Vec<i32> {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let me = std::process::id() as i32;
    let mut killed = Vec::new();
    for (pid, cwd) in process_cwds() {
        if pid == me || !cwd.starts_with(&home) {
            continue;
        }
        // SAFETY: plain syscall with a pid and a signal number.
        if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
            let err = std::io::Error::last_os_error();
            // Exited between listing and kill: nothing left to do.
            if err.raw_os_error() != Some(libc::ESRCH) {
                panic!("failed to kill pid {pid} (cwd {}): {err}", cwd.display());
            }
            continue;
        }
        killed.push(pid);
    }
    killed
}

#[cfg(target_os = "linux")]
fn process_cwds() -> Vec<(i32, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        panic!("cannot list /proc");
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        // Unreadable or vanished entries are other users' or already gone.
        let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let bytes = cwd.as_os_str().as_bytes();
        let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
        out.push((pid, PathBuf::from(std::ffi::OsStr::from_bytes(bytes))));
    }
    out
}

#[cfg(target_os = "macos")]
fn process_cwds() -> Vec<(i32, PathBuf)> {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: libproc calls with correctly sized buffers.
    unsafe {
        let estimate = libc::proc_listallpids(std::ptr::null_mut(), 0);
        assert!(estimate > 0, "proc_listallpids sizing call failed");
        let mut pids: Vec<c_int> = vec![0; estimate as usize * 2];
        let filled = libc::proc_listallpids(
            pids.as_mut_ptr() as *mut c_void,
            (pids.len() * std::mem::size_of::<c_int>()) as c_int,
        );
        assert!(filled > 0, "proc_listallpids failed");
        pids.truncate((filled as usize).min(pids.len()));
        let mut out = Vec::new();
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as c_int;
            let got = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut libc::proc_vnodepathinfo as *mut c_void,
                size,
            );
            // <= 0: other users' processes, zombies, or already gone.
            if got <= 0 {
                continue;
            }
            let path = CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const c_char);
            out.push((
                pid,
                PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())),
            ));
        }
        out
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_cwds() -> Vec<(i32, PathBuf)> {
    unimplemented!("listing process working directories is only implemented for Linux and macOS")
}
