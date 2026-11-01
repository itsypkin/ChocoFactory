//! Helpers for the timing-sensitive unit tests (#98).
//!
//! The wait helpers (`wait_until`, `LOAD_ALLOWANCE`, `POLL_INTERVAL`) live in
//! the dev-only `chocofactory-test-support` crate so `choco`'s tests can use
//! them too; they are re-exported here so `crate::test_support::…` paths work.

use std::time::Duration;

pub(crate) use chocofactory_test_support::{LOAD_ALLOWANCE, wait_until};
// Kept so `crate::test_support::POLL_INTERVAL` still resolves; no caller in
// this crate uses it today.
#[allow(unused_imports)]
pub(crate) use chocofactory_test_support::POLL_INTERVAL;

/// For the few tests where a production timer must fire once and then must
/// *not* fire again before the fixture responds. Unlike `LOAD_ALLOWANCE`
/// this is a real upper bound on how slow the fixture may be, so it can't
/// be arbitrarily large without making the test slow; see the tests that
/// use it (#98).
pub(crate) const RESPONSE_MARGIN: Duration = Duration::from_secs(2);

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
pub(crate) fn write_executable(path: &std::path::Path, contents: impl AsRef<str>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "writing")]
    fn write_executable_panics_when_the_write_fails() {
        write_executable(
            std::path::Path::new("/nonexistent-dir-for-choco-test/x"),
            "#!/bin/sh\n",
        );
    }
}
