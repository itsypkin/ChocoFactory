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

// Each test crate that includes this module uses only part of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

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

// ---------------------------------------------------------------------------
// Raw stdout checks for `--json` (#361). These read the bytes `choco` wrote,
// not a lossy String, so a raw control byte or a bad byte is seen as such.
// ---------------------------------------------------------------------------

/// Fails on the first byte below 0x20 in `bytes` other than the LF that ends
/// the value (`final_lf` says whether one is allowed as the last byte).
/// DEL (0x7f) is legal JSON string content and is not rejected here.
pub fn assert_no_raw_control_bytes(label: &str, bytes: &[u8], final_lf: bool) {
    for (offset, &b) in bytes.iter().enumerate() {
        if b < 0x20 && !(final_lf && b == b'\n' && offset + 1 == bytes.len()) {
            panic!("{label}: raw control byte {b:#04x} at offset {offset}");
        }
    }
}

/// `bytes` must be exactly one JSON value followed by one LF, with no other
/// control byte; strictly parsed with no trimming or repair.
pub fn parse_json_value(label: &str, bytes: &[u8]) -> Value {
    assert!(!bytes.is_empty(), "{label}: stdout is empty");
    assert!(
        bytes.ends_with(b"\n"),
        "{label}: stdout does not end with a newline"
    );
    assert_no_raw_control_bytes(label, bytes, true);
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("{label}: stdout is not strict JSON ({e})"))
}

/// Newline-delimited JSON: every line LF-terminated, nonempty, and strictly
/// one complete value.
pub fn parse_ndjson(label: &str, bytes: &[u8]) -> Vec<Value> {
    assert!(!bytes.is_empty(), "{label}: stdout is empty");
    assert!(
        bytes.ends_with(b"\n"),
        "{label}: stdout does not end with a newline"
    );
    bytes[..bytes.len() - 1]
        .split(|&b| b == b'\n')
        .enumerate()
        .map(|(i, line)| {
            let mut framed = line.to_vec();
            framed.push(b'\n');
            assert!(!line.is_empty(), "{label}: line {i} is empty");
            parse_json_value(&format!("{label} (line {i})"), &framed)
        })
        .collect()
}

/// `choco`'s stdout as one JSON value; `label` names the command.
pub fn json_stdout(label: &str, out: &Output) -> Value {
    parse_json_value(label, &out.stdout)
}

/// An action under `--json` prints nothing on stdout.
pub fn assert_empty_stdout(label: &str, out: &Output) {
    assert!(
        out.stdout.is_empty(),
        "{label}: stdout must be empty, got {:?}; stderr {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(test)]
mod json_bytes_tests {
    use super::*;

    fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let err = std::panic::catch_unwind(f).expect_err("expected a panic");
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    #[test]
    fn an_interior_control_byte_is_named_with_its_offset() {
        let m = panic_message(|| {
            parse_json_value("task events", b"{\"a\":\"x\x1b[0m\"}\n");
        });
        assert_eq!(m, "task events: raw control byte 0x1b at offset 7");
        for (bytes, byte) in [
            (&b"{\"a\":\"x\ny\"}\n"[..], "0x0a"),
            (&b"{\"a\":\"x\ry\"}\n"[..], "0x0d"),
            (&b"{\"a\":\"x\ty\"}\n"[..], "0x09"),
            (&b"{\"a\":\"x\0y\"}\n"[..], "0x00"),
        ] {
            let m = panic_message(|| {
                parse_json_value("c", bytes);
            });
            assert!(
                m.contains(&format!("raw control byte {byte} at offset 7")),
                "{m}"
            );
        }
    }

    #[test]
    fn the_final_newline_and_escapes_pass_and_offsets_count_bytes() {
        let v = parse_json_value("c", b"{\"a\":\"\\u001b \\n\"}\n");
        assert_eq!(v["a"], "\u{1b} \n");
        // A two-byte char before the control byte: the offset counts bytes.
        let m = panic_message(|| {
            parse_json_value("c", "{\"a\":\"é\x01\"}\n".as_bytes());
        });
        assert_eq!(m, "c: raw control byte 0x01 at offset 8");
    }

    #[test]
    fn del_is_legal_but_invalid_utf8_still_fails_strict_parsing() {
        let v = parse_json_value("c", b"{\"a\":\"\x7f\"}\n");
        assert_eq!(v["a"], "\u{7f}");
        let m = panic_message(|| {
            parse_json_value("c", b"{\"a\":\"\xff\xfe\"}\n");
        });
        assert!(m.contains("not strict JSON"), "{m}");
    }

    #[test]
    fn a_missing_final_newline_or_empty_output_fails() {
        assert!(panic_message(|| drop(parse_json_value("c", b"{}"))).contains("newline"));
        assert!(panic_message(|| drop(parse_json_value("c", b""))).contains("empty"));
    }

    #[test]
    fn ndjson_needs_complete_lf_terminated_lines() {
        let v = parse_ndjson("w", b"{\"a\":1}\n{\"a\":2}\n");
        assert_eq!(v.len(), 2);
        assert!(
            panic_message(|| drop(parse_ndjson("w", b"{\"a\":1}\n{\"a\""))).contains("newline")
        );
        assert!(
            panic_message(|| drop(parse_ndjson("w", b"{\"a\":1}\n\n{\"a\":2}\n")))
                .contains("line 1 is empty")
        );
        // A pretty-printed value split over lines is not NDJSON.
        assert!(panic_message(|| drop(parse_ndjson("w", b"{\n\"a\":1}\n"))).contains("w (line 0)"));
        let m = panic_message(|| drop(parse_ndjson("w", b"{\"a\":1}\n{\"a\":\"\x1b\"}\n")));
        assert_eq!(m, "w (line 1): raw control byte 0x1b at offset 6");
    }
}
