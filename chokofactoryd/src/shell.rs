//! Runs a workflow stage's command to completion (design §5.2, P2-1).
//!
//! Its own module rather than a private helper in `engine.rs` because
//! P2-2's `poll` stage kind needs exactly this runner, just invoked
//! repeatedly on an interval — the only difference between the two kinds
//! is what they do with the result, not how the command is run.
//!
//! Deliberately knows nothing about tasks, stages, or the database: it
//! takes a command and a working directory and reports what happened.

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
        // Both the timeout path below and a dropped caller rely on this:
        // without it the child outlives the future that was awaiting it
        // and keeps running — and mutating the task's working copy —
        // unsupervised. Same convention as `adapter/claude.rs` and
        // `worktree.rs`.
        .kill_on_drop(true);

    let started = Instant::now();
    let child = cmd.spawn().map_err(ShellError::Spawn)?;

    let output = match timeout {
        // Dropping the `wait_with_output` future on elapse drops the child
        // with it, and `kill_on_drop` reaps it — there is no branch here
        // that leaves the process running.
        //
        // Whatever the child had printed before being killed goes with it:
        // `wait_with_output` buffers internally and only yields on
        // completion, so recovering partial output would mean draining both
        // pipes into our own buffers for every command, timeout or not.
        // Not worth it — a timed-out stage's useful signal is that it hung,
        // and that is reported.
        Some(limit) => match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(result) => result.map_err(ShellError::Io)?,
            Err(_elapsed) => {
                return Ok(ShellOutcome {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration: started.elapsed(),
                    timed_out: true,
                });
            }
        },
        None => child.wait_with_output().await.map_err(ShellError::Io)?,
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
        // `false` is a shell builtin but not a python one, so this only
        // exits 0 if the shebang was actually honored.
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
