//! Runs a workflow stage's command to completion (design §5.2, P2-1).
//!
//! Its own module rather than a private helper in `engine.rs` because
//! P2-2's `poll` stage kind needs exactly this runner, just invoked
//! repeatedly on an interval — the only difference between the two kinds
//! is what they do with the result, not how the command is run.
//!
//! Deliberately knows nothing about tasks, stages, or the database: it
//! takes a command and a working directory and reports what happened.
//!
//! The command inherits the daemon's environment wholesale, like every
//! other subprocess this codebase spawns (`adapter/claude.rs`,
//! `worktree.rs`) — a stage command is operator-authored YAML, trusted the
//! same way the daemon's own configuration is. Output is buffered in full
//! before any cap is applied, so a command that prints without bound is
//! bounded only by memory; that is the same exposure `worktree.rs` already
//! carries, and streaming it would mean draining both pipes by hand for
//! every command.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::workflow_def::ShellCommand;

/// What a command did. Note this is the *success* type even when the
/// command failed: a nonzero exit is a normal, expected result that the
/// caller maps to an `error` outcome, not an error in running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// `None` when the child was killed by a signal rather than exiting on
    /// its own — including the kill this module performs on `timed_out`.
    /// Never treated as success.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub timed_out: bool,
}

impl ShellOutcome {
    /// Exit code 0 and only exit code 0 (§5.2). A timed-out or
    /// signal-killed child has no exit code and is never a success.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[derive(Debug)]
pub enum ShellError {
    /// The command could not be started at all — no `sh` on PATH, or a
    /// `script_file` that isn't executable (`EACCES`, the "forgot
    /// `chmod +x`" case). Distinct from a command that ran and failed.
    Spawn(std::io::Error),
    /// Reading the child's stdout/stderr failed partway through.
    Io(std::io::Error),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::Spawn(err) => write!(f, "failed to start command: {err}"),
            ShellError::Io(err) => write!(f, "failed to read command output: {err}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// How long to wait for a killed process group to actually die before
/// giving up on collecting its output. SIGKILL is not refusable, so this
/// only matters for a process stuck in uninterruptible I/O — which must
/// not take the task down with it.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Kills a command's whole process group when dropped, unless disarmed.
///
/// `Command::kill_on_drop` only signals the process the daemon spawned. An
/// inline command is `sh -c "<line>"`, so for anything that isn't a single
/// leaf process — a pipeline, `make && npm test`, a script that backgrounds
/// work — the grandchildren survive and keep running in the task's working
/// directory. The stage meanwhile reports `error`, and a workflow with
/// `on: { error: <self> }` and a `loop_guard` promptly starts another copy
/// on top of the first.
///
/// Paired with `Command::process_group(0)`, which makes the child a group
/// leader whose pgid equals its pid.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn armed(pid: Option<u32>) -> Self {
        // `None` means the child already exited and tokio reaped it, so
        // there is no group left to signal.
        ProcessGroup(pid)
    }

    /// Signals the group now, and stops the drop path from signalling it a
    /// second time.
    fn kill(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_group(pid);
        }
    }

    /// The command is over on its own; there is nothing left to kill.
    /// Signalling anyway would risk hitting an unrelated process that has
    /// since been given the same pid.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

fn kill_group(pid: u32) {
    let Ok(pgid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: `killpg` is async-signal-safe and merely sends a signal; the
    // only failure modes are "no such group" and "not permitted", both of
    // which are reported through the return value rather than by any
    // memory effect. The pgid is the child's own pid, which is still
    // un-reusable here because tokio has not yet waited on it.
    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH just means the group is already gone, which is the common
        // and entirely fine case.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pgid, %err, "failed to kill a shell command's process group");
        }
    }
}

/// Runs `command` in `cwd` to completion, or kills it once `timeout`
/// elapses.
///
/// An inline `command:` goes through `sh -c` because the design's own
/// examples are shell pipelines (§5.1's `... | sort -u`), so treating the
/// string as an argv array would break them. A `script_file:` is spawned
/// directly instead, so its `#!` line chooses the interpreter rather than
/// everything silently running under `sh`; the loader has already resolved
/// it to an existing absolute path.
pub async fn run(
    command: &ShellCommand,
    cwd: &Path,
    timeout: Option<Duration>,
) -> Result<ShellOutcome, ShellError> {
    let mut cmd = match command {
        ShellCommand::Inline(line) => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(line);
            cmd
        }
        ShellCommand::ScriptFile(path) => Command::new(path),
    };

    cmd.current_dir(cwd)
        // Nothing is ever written to a stage command's stdin, and leaving
        // it inherited would let a command that reads stdin block forever
        // on the daemon's own (matches `worktree.rs`'s `run_git_stdout`).
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `kill_on_drop` alone only reaps the process the daemon spawned —
        // for an inline command that's the `sh`, not the pipeline or build
        // it started. Putting the command in its own process group lets
        // `ProcessGroup` below signal the whole tree, so a timed-out
        // `make && npm test` can't leave grandchildren running in the
        // task's working copy while the workflow retries the same command
        // on top of them.
        .process_group(0)
        .kill_on_drop(true);

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(ShellError::Spawn)?;

    // Armed until the command is known to be over: every early return below
    // (an I/O failure, the caller dropping this future mid-run) then reaps
    // the whole group on the way out rather than orphaning it.
    let mut group = ProcessGroup::armed(child.id());
    // `stdin` is `Stdio::null()`, so nothing can be blocked writing to the
    // child; dropping our handle just closes an unused pipe.
    drop(child.stdin.take());
    let wait = child.wait_with_output();
    tokio::pin!(wait);

    let output = match timeout {
        Some(limit) => match tokio::time::timeout(limit, &mut wait).await {
            Ok(result) => {
                let output = result.map_err(ShellError::Io)?;
                group.disarm();
                output
            }
            Err(_elapsed) => {
                group.kill();
                // The pipes are still open, so finishing the wait both
                // reaps the zombie and yields whatever the command managed
                // to print before it died — the most useful thing an
                // operator gets from a hung stage. Bounded, because a
                // process wedged in uninterruptible I/O won't die even on
                // SIGKILL and must not wedge this task with it.
                let collected = tokio::time::timeout(REAP_GRACE, &mut wait).await;
                group.disarm();
                let (stdout, stderr) = match collected {
                    Ok(Ok(output)) => (
                        String::from_utf8_lossy(&output.stdout).into_owned(),
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    ),
                    _ => (String::new(), String::new()),
                };
                return Ok(ShellOutcome {
                    exit_code: None,
                    stdout,
                    stderr,
                    duration: started.elapsed(),
                    timed_out: true,
                });
            }
        },
        None => {
            let output = wait.await.map_err(ShellError::Io)?;
            group.disarm();
            output
        }
    };

    Ok(ShellOutcome {
        exit_code: output.status.code(),
        // Lossy rather than a hard error: a command whose output isn't
        // valid UTF-8 still ran, and its exit code is what decides the
        // stage's outcome. Matches `worktree.rs`'s handling of git output.
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        duration: started.elapsed(),
        timed_out: false,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn inline(line: &str) -> ShellCommand {
        ShellCommand::Inline(line.to_string())
    }

    /// Scratch directory that cleans itself up, mirroring the local
    /// `TempDir` helpers already used in `engine.rs`/`workflow_def.rs`
    /// (the repo has no `tempfile` dependency).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("choko-shell-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn captures_stdout_of_a_successful_command() {
        let outcome = run(&inline("printf 'hello'"), &std::env::temp_dir(), None)
            .await
            .unwrap();

        assert!(outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, "hello");
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn reports_a_nonzero_exit_with_its_stderr() {
        let outcome = run(
            &inline("printf 'boom' >&2; exit 3"),
            &std::env::temp_dir(),
            None,
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.stderr, "boom");
    }

    /// §5.1's own example command is a pipeline, so the runner must go
    /// through a shell rather than treating the string as an argv array.
    #[tokio::test]
    async fn runs_an_inline_command_through_a_shell() {
        let outcome = run(
            &inline("printf 'b\\na\\na\\n' | sort -u | tr -d '\\n'"),
            &std::env::temp_dir(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(outcome.stdout, "ab");
    }

    #[tokio::test]
    async fn runs_the_command_in_the_given_directory() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("marker"), b"x").unwrap();

        let outcome = run(&inline("ls"), dir.path(), None).await.unwrap();

        assert_eq!(outcome.stdout.trim(), "marker");
    }

    #[tokio::test]
    async fn kills_a_command_that_outlives_its_timeout() {
        let outcome = run(
            &inline("sleep 30"),
            &std::env::temp_dir(),
            Some(Duration::from_millis(100)),
        )
        .await
        .unwrap();

        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code, None);
        // The wait returned on the timeout rather than after the sleep.
        assert!(outcome.duration < Duration::from_secs(5));
    }

    /// The timeout has to reap the command's whole process tree, not just
    /// the `sh` we spawned. Otherwise a timed-out pipeline or build keeps
    /// running in the task's working copy while the workflow's `on: error`
    /// edge starts another copy on top of it.
    ///
    /// The grandchild here touches a file after a delay; if it survived the
    /// timeout, that file would appear.
    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group_not_just_the_shell() {
        let dir = TempDir::new();
        let marker = dir.path().join("grandchild-survived");

        let outcome = run(
            &inline(&format!(
                "sh -c 'sleep 2; touch {}' & wait",
                marker.display()
            )),
            dir.path(),
            Some(Duration::from_millis(1)),
        )
        .await
        .unwrap();
        assert!(outcome.timed_out);

        // Well past when the grandchild would have fired had it lived.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived the timeout and kept working in the task's cwd"
        );
    }

    /// A killed command's partial output is still worth reporting — it is
    /// often the only clue about where the command hung.
    #[tokio::test]
    async fn a_timed_out_command_still_reports_what_it_printed() {
        let outcome = run(
            &inline("printf 'got this far'; sleep 30"),
            &std::env::temp_dir(),
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();

        assert!(outcome.timed_out);
        assert_eq!(outcome.stdout, "got this far");
    }

    #[tokio::test]
    async fn a_command_finishing_inside_its_timeout_is_not_timed_out() {
        let outcome = run(
            &inline("printf 'fast'"),
            &std::env::temp_dir(),
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out);
        assert_eq!(outcome.stdout, "fast");
    }

    /// A `script_file` is spawned directly, so its `#!` line picks the
    /// interpreter — running everything under `sh` would break any script
    /// that isn't a shell script.
    #[tokio::test]
    async fn runs_a_script_file_under_its_own_shebang() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let script = dir.path().join("script");
        // `print(...)` is a syntax error in sh, so this only produces the
        // expected stdout if the shebang was actually honored.
        std::fs::write(&script, "#!/usr/bin/env python3\nprint('from python')\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = run(
            &ShellCommand::ScriptFile(script),
            &std::env::temp_dir(),
            None,
        )
        .await
        .unwrap();

        assert!(outcome.succeeded());
        assert_eq!(outcome.stdout.trim(), "from python");
    }

    #[tokio::test]
    async fn a_non_executable_script_file_is_a_spawn_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let script = dir.path().join("script");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = run(
            &ShellCommand::ScriptFile(script),
            &std::env::temp_dir(),
            None,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ShellError::Spawn(_)), "got {err:?}");
        // The message has to name the reason, since it's the only clue an
        // operator gets that a chmod is missing.
        assert!(err.to_string().contains("failed to start command"));
    }

    #[tokio::test]
    async fn non_utf8_output_does_not_fail_the_run() {
        let outcome = run(&inline("printf '\\377\\376'"), &std::env::temp_dir(), None)
            .await
            .unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.stdout.is_empty());
    }

    #[tokio::test]
    async fn a_signal_killed_command_has_no_exit_code() {
        let outcome = run(&inline("kill -TERM $$"), &std::env::temp_dir(), None)
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, None);
        assert!(!outcome.succeeded());
        assert!(!outcome.timed_out);
    }
}
