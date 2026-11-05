//! Helpers for the timing-sensitive unit tests (#98).
//!
//! The wait helpers (`wait_until`, `LOAD_ALLOWANCE`, `POLL_INTERVAL`) live in
//! the dev-only `chocofactory-test-support` crate so `choco`'s tests can use
//! them too; they are re-exported here so `crate::test_support::…` paths work.

use std::time::Duration;

pub(crate) use chocofactory_test_support::{LOAD_ALLOWANCE, wait_until, write_executable};
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

/// Fixture steps for a turn that reports, then leaves an owned escaped job
/// behind: the agent writes its own pid to `leader_pid_file`, starts the job
/// (writing its pid to `job_pid_file`), reports, ends its turn and stays up.
pub(crate) fn escaped_job_turn_steps(
    leader_pid_file: &std::path::Path,
    job_pid_file: &std::path::Path,
) -> serde_json::Value {
    serde_json::json!([
        {"op": "read_turn"},
        {"op": "run", "command": format!("echo $PPID > '{}'", leader_pid_file.display())},
        {"op": "spawn_escaped", "setsid": true, "double_fork": true, "pid_file": job_pid_file},
        {"op": "report", "outcome": "done"},
        {"op": "result"},
        {"op": "sleep", "seconds": 60},
    ])
}

/// A process-table reader for the late-discovery turn: the pre-kill scans see
/// nothing (so the group classifies as having nothing alive), the first sweep
/// scan holds only the agent as a live member of its own group, and every
/// later scan is the real table, where the escaped job is owned through its
/// turn marker. The agent's pid is read from `leader_pid_file`.
pub(crate) fn late_discovery_reader(
    leader_pid_file: std::path::PathBuf,
) -> crate::proc_table::ProcReader {
    let sweeps = std::sync::atomic::AtomicUsize::new(0);
    std::sync::Arc::new(move |marker| {
        let Some(marker) = marker else {
            return Ok(Vec::new());
        };
        if sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            return crate::proc_table::read(Some(marker));
        }
        let pid: i32 = std::fs::read_to_string(&leader_pid_file)?
            .trim()
            .parse()
            .map_err(std::io::Error::other)?;
        Ok(vec![crate::proc_table::ProcEntry {
            pid,
            ppid: std::process::id() as i32,
            pgid: pid,
            sid: pid,
            // SAFETY: only reads the process's own uid.
            uid: unsafe { libc::geteuid() },
            start: 1,
            zombie: false,
            comm: "leader".into(),
            marker: crate::proc_table::MarkerStatus::Absent,
        }])
    })
}

/// Kills the process whose pid is in the file when dropped, so a test that
/// fails (or never gets that far) leaves no orphaned job behind. Declare it
/// after the test's temporary directory, so it drops first and the file is
/// still there. A missing or unparseable file means there is nothing to kill.
pub(crate) struct KillPidFileOnDrop(pub(crate) std::path::PathBuf);

impl Drop for KillPidFileOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 1)
        {
            // SAFETY: signals only the pid the test's own fixture wrote.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}
