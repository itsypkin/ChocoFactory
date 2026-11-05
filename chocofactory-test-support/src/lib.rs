//! Helpers shared by the tests of `choco` and `chocofactoryd`: the waits for
//! timing-sensitive tests (#98) and the fork-safe executable writers.
//! Dev-dependency only: nothing here enters a shipped binary.

use std::time::Duration;

/// How long a test waits for something the code under test does
/// asynchronously (a subprocess starting, an event being written, a stage
/// being entered) before declaring it never happened.
///
/// This is a load allowance, not a deadline, and nothing asserts against
/// it: a wait returns the moment its condition holds, so a passing test is
/// no slower for it being large. It only has to outlast the worst
/// scheduling delay a loaded machine produces, since `cargo test` runs the
/// suite in parallel and often alongside other builds (#98).
pub const LOAD_ALLOWANCE: Duration = Duration::from_secs(30);

/// How often a wait re-checks its condition.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Re-runs `check` every `POLL_INTERVAL` until it returns `Ok`, and returns
/// that value. `Err(last_seen)` means "not yet", and carries a description
/// of what the check saw this time. Panics after `LOAD_ALLOWANCE` with
/// "timed out after {LOAD_ALLOWANCE:?} waiting for {what}; last saw: {last_seen}".
///
/// A check that reads a database, a file or a process must panic on a
/// failed read and return `Err` only for "not yet", so a broken read can't
/// look like a timeout.
pub async fn wait_until<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        let last_seen = match check().await {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {LOAD_ALLOWANCE:?} waiting for {what}; last saw: {last_seen}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `wait_until` for sync tests: the same contract, constants and panic
/// text, blocking the thread between checks. Never call it from an async
/// test; it would block the runtime.
pub fn wait_until_blocking<T>(what: &str, check: impl FnMut() -> Result<T, String>) -> T {
    wait_until_blocking_within(LOAD_ALLOWANCE, what, check)
}

/// `wait_until_blocking` with a caller-chosen bound, for a wait whose
/// worst case is known to need more than `LOAD_ALLOWANCE`. The panic text
/// names `allowance`.
pub fn wait_until_blocking_within<T>(
    allowance: Duration,
    what: &str,
    mut check: impl FnMut() -> Result<T, String>,
) -> T {
    let deadline = std::time::Instant::now() + allowance;
    loop {
        let last_seen = match check() {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        if std::time::Instant::now() >= deadline {
            panic!("timed out after {allowance:?} waiting for {what}; last saw: {last_seen}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Writes `contents` to `path` as an executable (mode 0o755) file without
/// this process ever holding a write handle on it.
///
/// A test that writes a script with `std::fs::write` and then execs it can
/// fail with ETXTBSY ("Text file busy") on Linux: while the write
/// descriptor is open, another test thread may fork, and the child inherits
/// the descriptor until it execs. `execve` of a file open for writing by any
/// process fails. Here `/bin/sh` does the write and the chmod, so the test
/// process only holds a pipe; once the child has exited the file has no
/// writers and a later exec cannot see ETXTBSY.
///
/// Panics (naming the path) on any spawn, write or non-success exit.
pub fn write_executable(path: &std::path::Path, contents: impl AsRef<str>) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning /bin/sh to write {}: {e}", path.display()));
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let written = stdin.write_all(contents.as_ref().as_bytes());
    drop(stdin);
    let status = child
        .wait()
        .unwrap_or_else(|e| panic!("waiting for the writer of {}: {e}", path.display()));
    written.unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

/// Copies an executable so that a later exec of `to` cannot see ETXTBSY, for
/// the reason in [`write_executable`]: `/bin/cp` holds the write handle, not
/// this process, and it has exited by the time this returns. `std::fs::copy`
/// holds it here, for as long as the copy takes.
///
/// Panics (naming both paths) on a spawn failure or a non-success exit.
pub fn copy_executable(from: &std::path::Path, to: &std::path::Path) {
    let name = || format!("{} to {}", from.display(), to.display());
    let status = std::process::Command::new("/bin/cp")
        .arg(from)
        .arg(to)
        .status()
        .unwrap_or_else(|e| panic!("spawning /bin/cp to copy {}: {e}", name()));
    assert!(status.success(), "copying {} failed: {status}", name());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read};
    use std::process::Command;

    fn panic_message(err: Box<dyn std::any::Any + Send>) -> String {
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn returns_the_first_ok_without_sleeping() {
        let start = tokio::time::Instant::now();
        let v = wait_until("x", || async { Ok::<_, String>(7) }).await;
        assert_eq!(v, 7);
        assert_eq!(tokio::time::Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_the_value_from_the_call_that_turned_ok() {
        let mut calls = 0;
        let v = wait_until("x", || {
            calls += 1;
            let n = calls;
            async move {
                if n == 3 {
                    Ok(n)
                } else {
                    Err(format!("call {n}"))
                }
            }
        })
        .await;
        assert_eq!(v, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn panics_after_the_load_allowance_naming_what_and_last_seen() {
        let start = tokio::time::Instant::now();
        let handle = tokio::spawn(async {
            let _: () = wait_until("the thing", || async { Err("still no".to_string()) }).await;
        });
        let err = handle.await.unwrap_err();
        assert!(err.is_panic());
        let msg = panic_message(err.into_panic());
        assert!(
            msg.contains("waiting for the thing; last saw: still no"),
            "{msg}"
        );
        assert!(tokio::time::Instant::now() - start >= LOAD_ALLOWANCE);
    }

    #[test]
    fn blocking_returns_the_first_ok_after_one_call() {
        let mut calls = 0;
        let v = wait_until_blocking("x", || {
            calls += 1;
            Ok::<_, String>(7)
        });
        assert_eq!((v, calls), (7, 1));
    }

    #[test]
    fn blocking_returns_the_value_from_the_call_that_turned_ok() {
        let mut calls = 0;
        let v = wait_until_blocking("x", || {
            calls += 1;
            if calls == 3 {
                Ok(calls)
            } else {
                Err(format!("call {calls}"))
            }
        });
        assert_eq!(v, 3);
    }

    #[test]
    fn blocking_panics_after_the_allowance_naming_what_and_last_seen() {
        let err = std::panic::catch_unwind(|| {
            let _: () = wait_until_blocking_within(Duration::from_millis(50), "the thing", || {
                Err("still no".to_string())
            });
        })
        .unwrap_err();
        let msg = panic_message(err);
        assert!(
            msg.contains("waiting for the thing; last saw: still no"),
            "{msg}"
        );
    }

    #[test]
    #[should_panic(expected = "writing")]
    fn write_executable_panics_when_the_write_fails() {
        write_executable(
            std::path::Path::new("/nonexistent-dir-for-choco-test/x"),
            "#!/bin/sh\n",
        );
    }

    #[test]
    #[should_panic(expected = "copying")]
    fn copy_executable_panics_when_the_copy_fails() {
        copy_executable(
            std::path::Path::new("/bin/sh"),
            std::path::Path::new("/nonexistent-dir-for-choco-test/x"),
        );
    }

    /// A child between fork and exec holds a copy of every descriptor this
    /// process has open, as a sibling test thread's spawn does for a moment.
    /// `write_to` writes more than a FIFO holds, so the writer stays blocked
    /// with its handle open, and a real child looks through its own descriptors
    /// for a write handle on that FIFO. One here means a sibling's fork could
    /// make the file busy for an exec (on Linux; macOS has no such refusal).
    fn assert_a_fork_inherits_no_write_handle(
        name: &str,
        write_to: impl FnOnce(&std::path::Path) + Send,
    ) {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        use std::os::unix::process::CommandExt;

        let dir =
            std::env::temp_dir().join(format!("choco-test-support-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success(), "mkfifo: {made}");
        let ino = std::fs::metadata(&fifo).unwrap().ino();
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();

        let found = std::thread::scope(|scope| {
            // Owned here, so a panic below closes it and frees a blocked writer.
            let mut reader = reader;
            let writer = scope.spawn(|| write_to(&fifo));
            // A byte in the FIFO means the writer has its handle open.
            wait_until_blocking("the writer to start", || match reader.read(&mut [0u8]) {
                Ok(1) => Ok(()),
                Ok(_) => Err("no writer yet".to_string()),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Err("no data yet".to_string()),
                Err(e) => panic!("reading the FIFO: {e}"),
            });

            let (mut result_r, result_w) = std::io::pipe().unwrap();
            let result_fd = result_w.as_raw_fd();
            let mut child = Command::new("/bin/sh");
            child.args(["-c", ":"]);
            // SAFETY: the closure only calls fstat(2), fcntl(2) and write(2).
            unsafe {
                child.pre_exec(move || {
                    let mut found = 0u8;
                    for fd in 0..1024 {
                        let mut st = std::mem::zeroed::<libc::stat>();
                        if libc::fstat(fd, &raw mut st) == 0
                            && st.st_ino == ino
                            && libc::fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE != libc::O_RDONLY
                        {
                            found = 1;
                        }
                    }
                    libc::write(result_fd, (&raw const found).cast(), 1);
                    Ok(())
                });
            }
            let status = child.status().unwrap();
            assert!(status.success(), "the checking child: {status}");
            drop(result_w);
            let mut found = [0u8];
            result_r.read_exact(&mut found).unwrap();

            let mut buf = vec![0u8; 1 << 16];
            wait_until_blocking("the writer to finish", || match reader.read(&mut buf) {
                Ok(0) => Ok(()),
                Ok(n) => Err(format!("{n} more bytes")),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Err("no data yet".to_string()),
                Err(e) => panic!("reading the FIFO: {e}"),
            });
            writer.join().unwrap();
            found[0]
        });
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            found, 0,
            "{name}: a child between fork and exec inherited a write handle"
        );
    }

    /// More than any FIFO on Linux or macOS holds.
    const BIGGER_THAN_A_FIFO: usize = 1 << 18;

    #[test]
    fn a_fork_in_flight_inherits_no_write_handle_from_write_executable() {
        assert_a_fork_inherits_no_write_handle("write", |path| {
            write_executable(path, "x".repeat(BIGGER_THAN_A_FIFO));
        });
    }

    #[test]
    fn a_fork_in_flight_inherits_no_write_handle_from_copy_executable() {
        let source =
            std::env::temp_dir().join(format!("choco-test-support-{}-source", std::process::id()));
        write_executable(&source, "x".repeat(BIGGER_THAN_A_FIFO));
        assert_a_fork_inherits_no_write_handle("copy", |path| copy_executable(&source, path));
        std::fs::remove_file(&source).unwrap();
    }
}
