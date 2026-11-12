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

/// Captures what the code under test logs on this thread, for as long as
/// the guard [`LogBuf::capture`] returns lives. Every test that asserts on a
/// log line captures it through here.
#[derive(Clone, Default)]
pub(crate) struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

impl LogBuf {
    /// Makes this buffer the current thread's subscriber until the guard
    /// drops.
    ///
    /// `tracing` decides once, when a log line is first reached anywhere in
    /// the process, whether any subscriber wants it, and caches that. While
    /// exactly one `Dispatch` exists it asks only the default of the thread
    /// that got there first. A test thread with no capture answers "never",
    /// and the answer sticks for every thread, so a capture alive at that
    /// moment silently loses the line. That is how the pause-settle test
    /// lost its ERROR line when a sibling test reached the same line first.
    ///
    /// `KEEPER`, a `Dispatch` that wants nothing and lives as long as the
    /// process, keeps the count above one whenever a capture exists. The
    /// cache then asks every live `Dispatch`. "Never" from the keeper and
    /// "always" from the capture make "sometimes", which is checked against
    /// the current thread's subscriber on each event.
    pub(crate) fn capture(&self) -> tracing::subscriber::DefaultGuard {
        self.capture_at(tracing::Level::INFO)
    }

    /// [`LogBuf::capture`], keeping lines down to `level` (the plain
    /// capture keeps INFO and above). A test that asserts on a DEBUG line
    /// captures through here, so it gets the same `KEEPER`.
    pub(crate) fn capture_at(&self, level: tracing::Level) -> tracing::subscriber::DefaultGuard {
        static KEEPER: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
        KEEPER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(level)
                .finish(),
        )
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pause-settle failure, with its order forced. A thread with no
    /// capture reaches a log line first, and the capture alive at that
    /// moment must still get the line when its own thread reaches it. The
    /// line's callsite is this test's own, so the uncaptured thread's call
    /// is its first hit in the process, whatever else runs.
    ///
    /// Without `KEEPER` this fails whenever this capture is the only live
    /// `Dispatch`, always when the test runs alone. With another test's
    /// capture alive at that moment it would pass without `KEEPER` too.
    #[test]
    fn a_capture_keeps_a_line_an_uncaptured_thread_reached_first() {
        fn line() {
            tracing::error!("a line an uncaptured thread reached first");
        }
        let logs = LogBuf::default();
        let _capture = logs.capture();
        std::thread::spawn(line).join().unwrap();
        line();
        let text = logs.text();
        assert_eq!(
            text.matches("a line an uncaptured thread reached first")
                .count(),
            1,
            "the capture should hold its own thread's line, once: {text:?}"
        );
    }
}

/// A scripted run of `tests/fixtures/fake_omp.py` (see its docstring): the
/// wrapper binary to hand to an `OmpAdapter`, the gates the test releases
/// and the log of what the fixture really did.
pub(crate) struct OmpScript {
    pub binary: String,
    gates: std::path::PathBuf,
    log: std::path::PathBuf,
    prompts: std::path::PathBuf,
}

impl OmpScript {
    /// Writes the script and a wrapper under `dir` (any existing directory).
    /// The wrapper answers `--version` itself, like the adapter tests'.
    pub(crate) fn new(
        dir: &std::path::Path,
        steps: serde_json::Value,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let root = dir.join(format!("omp-script-{}", uuid::Uuid::new_v4()));
        let gates = root.join("gates");
        std::fs::create_dir_all(&gates).unwrap();
        let script = root.join("script.json");
        std::fs::write(&script, steps.to_string()).unwrap();
        let log = root.join("progress.log");
        let prompts = root.join("prompts.log");
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake_omp.py");
        let mut text = String::from("#!/bin/sh\n[ \"$1\" = --choco-warm-up ] && exit 0\n");
        let mut vars = vec![
            ("FAKE_OMP_SCRIPT", script.display().to_string()),
            ("FAKE_OMP_GATE_DIR", gates.display().to_string()),
            ("FAKE_OMP_LOG", log.display().to_string()),
            ("FAKE_OMP_PROMPT_LOG", prompts.display().to_string()),
        ];
        vars.extend(extra_env.iter().map(|(k, v)| (*k, v.to_string())));
        for (key, value) in vars {
            text.push_str(&format!("export {key}='{}'\n", value.replace('\'', "'\\''")));
        }
        text.push_str("case \"$1\" in --version) echo 'omp/fake-1.0'; exit 0;; esac\n");
        text.push_str(&format!("exec '{}' \"$@\"\n", fixture.display()));
        let wrapper = root.join("omp-wrapper");
        write_executable(&wrapper, text);
        // The first exec of a fresh file pays a one-time Gatekeeper check on
        // macOS; pay it here, outside the adapter's version-probe timeout.
        let status = std::process::Command::new(&wrapper)
            .arg("--choco-warm-up")
            .status()
            .unwrap();
        assert!(status.success());
        Self {
            binary: wrapper.display().to_string(),
            gates,
            log,
            prompts,
        }
    }

    /// Lets the fixture past the gate named `name`.
    pub(crate) fn release(&self, name: &str) {
        std::fs::write(self.gates.join(name), "").unwrap();
    }

    pub(crate) fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Waits for the fixture to log `tag`; fails with the whole log.
    pub(crate) async fn wait_log(&self, tag: &str) {
        let log = self.log.clone();
        let tag = tag.to_string();
        wait_until(&format!("the fixture to log {tag}"), || async {
            let lines: Vec<String> = std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            if lines.iter().any(|line| *line == tag) {
                Ok(())
            } else {
                Err(format!("{lines:?}"))
            }
        })
        .await
    }

    /// The pid the fixture logged for itself (its `pid` step).
    pub(crate) fn pid(&self) -> u32 {
        self.lines()
            .iter()
            .find_map(|line| line.strip_prefix("pid:"))
            .and_then(|pid| pid.parse().ok())
            .expect("the fixture logged its pid")
    }

    /// Waits until the fixture has received at least `count` prompts.
    pub(crate) async fn wait_prompts(&self, count: usize) {
        let prompts = self.prompts.clone();
        wait_until(&format!("{count} prompts"), || async {
            let seen = std::fs::read_to_string(&prompts)
                .unwrap_or_default()
                .lines()
                .count();
            if seen >= count {
                Ok(())
            } else {
                Err(format!("{seen} prompts"))
            }
        })
        .await
    }

    /// How many prompts the fixture has received.
    pub(crate) fn prompts_received(&self) -> usize {
        std::fs::read_to_string(&self.prompts)
            .unwrap_or_default()
            .lines()
            .count()
    }
}
