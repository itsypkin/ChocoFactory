use super::runners::DetachedKind;
use super::stage_capture::derive_capture;
use super::*;

/// How much of a command's stdout/stderr goes onto the timeline. Small on
/// purpose: this is a human-facing breadcrumb, and the full output isn't
/// retained anywhere.
pub(super) const EVENT_OUTPUT_TAIL_BYTES: usize = 2048;

/// Logs a poll attempt whose process group outlived its kill.
///
/// `run_shell_stage` warns on the same condition, and an operator grepping
/// logs for escaped process groups should find both kinds — a poll is if
/// anything the likelier source, since it can kill an attempt on every
/// interval for as long as its budget lasts.
pub(super) fn warn_if_escaped(task_id: &str, stage_name: &str, outcome: &shell::ShellOutcome) {
    if outcome.escaped {
        tracing::warn!(
            task_id,
            stage = stage_name,
            "a poll attempt was killed but its process group could not be confirmed dead"
        );
    }
}

/// A duration as whole milliseconds, saturating rather than wrapping — a
/// nonsense number on the timeline is worse than a clamped one.
pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn elapsed_ms(since: std::time::Instant) -> u64 {
    duration_ms(since.elapsed())
}

/// The trailing `EVENT_OUTPUT_TAIL_BYTES` of `text`, trimmed. The *tail*
/// rather than the head because a failing command's actual error is
/// almost always the last thing it printed.
pub(super) fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= EVENT_OUTPUT_TAIL_BYTES {
        return trimmed.to_string();
    }
    // Walk back to a char boundary so a multi-byte char isn't split.
    let mut start = trimmed.len() - EVENT_OUTPUT_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

/// How a command is named on the timeline. A `script_file` shows its path;
/// there's no meaningful "command line" to display for one.
pub(super) fn describe_command(command: &ShellCommand) -> String {
    match command {
        ShellCommand::Inline(line) => line.clone(),
        ShellCommand::ScriptFile(path) => path.display().to_string(),
    }
}

impl WorkflowEngine {
    /// Starts a `shell` stage's command (§5.2) and returns immediately; the
    /// outcome arrives later, from the detached runner below.
    ///
    /// Running the command inline here instead would deadlock the task
    /// permanently: `enter_stage` is called from inside the per-task lock
    /// held by `advance`/`start_task`, the runner has to call `advance` to
    /// report its outcome, and `tokio::sync::Mutex` is not reentrant. The
    /// `agent_turn` path has the same constraint and resolves it the same
    /// way, via `spawn_turn_watcher`.
    pub(super) async fn enter_shell(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            ..
        } = *entry;
        let StageKind::Shell {
            command,
            capture,
            timeout,
            env,
        } = &entry.stage_def.kind
        else {
            unreachable!("enter_shell is only called for Shell stages")
        };
        let capture = *capture;
        let timeout = *timeout;
        let (command, env) = self.render_stage_command(entry, command, env).await?;
        // Resolved here rather than in the spawned task so that a missing
        // task fails the transition that caused it, where the caller can
        // still see the error, instead of only reaching a log line.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let cwd = working_dir(&task, definition)?;

        self.spawn_shell_runner(
            task_id.to_string(),
            Arc::clone(definition),
            stage_name.to_string(),
            command,
            capture,
            timeout,
            cwd,
            env,
        );
        Ok(())
    }

    /// Deliberately a *synchronous* fn, like `spawn_turn_watcher`. The
    /// spawned future eventually calls `advance` → `enter_stage` →
    /// `enter_shell`, i.e. back to here; discharging `tokio::spawn`'s
    /// `Send` obligation from inside an `async fn` would make that cycle
    /// part of the compiler's auto-trait inference for `enter_shell`'s own
    /// future and fail to resolve. A sync fn's body is checked
    /// independently, which breaks the cycle.
    #[allow(clippy::too_many_arguments)]
    fn spawn_shell_runner(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        command: ShellCommand,
        capture: Option<Capture>,
        timeout: Option<Duration>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        let engine = Arc::clone(self);
        // Registered so `cancel_task` can abort this runner and kill the
        // command it's running (#69) — a `shell` stage has no `session`,
        // so killing the task's agent session would not reach it.
        let registered_task_id = task_id.clone();
        let spawned =
            self.spawn_registered_runner(&registered_task_id, move |runner_id| async move {
                engine
                    .run_shell_stage(
                        &task_id,
                        &definition,
                        &stage_name,
                        command,
                        capture,
                        timeout,
                        cwd,
                        env,
                    )
                    .await;
                engine.finish_runner(&task_id, runner_id);
            });
        if !spawned {
            tracing::warn!(
                task_id = %registered_task_id,
                "daemon is shutting down; shell stage not started, the next start recovers it"
            );
        }
    }

    /// Runs the command, records what it did on the task's timeline, and
    /// advances the task with `done`/`error` (§5.2) carrying any capture.
    #[allow(clippy::too_many_arguments)]
    async fn run_shell_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        command: ShellCommand,
        capture: Option<Capture>,
        timeout: Option<Duration>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        let described = describe_command(&command);

        // A failure to run the command at all is reported as the stage's
        // `error` outcome rather than dropped: from the workflow's point of
        // view "the command could not be run" and "the command ran and
        // failed" both mean this stage did not succeed, and a task whose
        // `on: error` edge exists should follow it either way. The reason
        // goes on the timeline, since it's the only place an operator would
        // find it.
        let started = std::time::Instant::now();
        let outcome = match shell::run(&command, &cwd, timeout, &env).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // The two variants mean materially different things to
                // whoever reads this: `Spawn` means nothing ran, while `Io`
                // means the command *did* run — possibly for a long time,
                // possibly mutating the working copy — and only reading its
                // output failed. Reporting both as "could not be started"
                // would actively mislead. Two call sites rather than one
                // interpolated message, so each stays a static string that
                // log aggregation can group on.
                match err {
                    shell::ShellError::Spawn(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command could not be started"
                    ),
                    shell::ShellError::Io(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command ran but its output could not be read"
                    ),
                }
                self.append_command_event(
                    task_id,
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "exit_code": Value::Null,
                        "timed_out": false,
                        "duration_ms": elapsed_ms(started),
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "note": err.to_string(),
                    }),
                )
                .await;
                self.finish_detached(
                    DetachedKind::Shell,
                    task_id,
                    definition,
                    stage_name,
                    "error",
                    None,
                )
                .await;
                return;
            }
        };

        let stage_outcome = if outcome.succeeded() { "done" } else { "error" };

        // Only a command that succeeded contributes a capture. A failed one
        // has nothing worth handing to a later stage, and storing it anyway
        // would be actively harmful: `stages.<name>` is keyed by stage, so
        // a stage re-entered by a retry loop would overwrite the good value
        // from the attempt that worked with the failed attempt's output (or,
        // on a timeout, with an empty string). A later
        // `{{ stages.open_pr.number }}` would then resolve against garbage.
        // The command's output is still on the timeline either way.
        let (captured, mut note) = if outcome.succeeded() {
            derive_capture(capture, &outcome.stdout, task_id, stage_name, "stdout")
        } else if capture.is_some() {
            (
                None,
                Some("stdout not captured: the command did not succeed".to_string()),
            )
        } else {
            (None, None)
        };

        if outcome.timed_out {
            tracing::warn!(
                task_id,
                stage = stage_name,
                escaped = outcome.escaped,
                "shell stage command exceeded its timeout and was killed"
            );
        }
        // Outranks any capture note: the workflow is about to follow its
        // `on: error` edge, quite possibly straight back into this same
        // command, while the last one is still running in the same working
        // copy. The timeline is where an operator would find that out.
        //
        // Worded for what both `escaped` arms actually know — one saw the
        // group outlive SIGKILL, the other only failed to read its pipes —
        // and kept short enough to survive `choco task events`' 100-char
        // line budget, since being truncated before "may still be running"
        // would defeat the point of recording it.
        if outcome.escaped {
            note = Some(
                "could not confirm the process group exited — something may still be running"
                    .to_string(),
            );
        }

        let mut payload = json!({
            "stage": stage_name,
            "command": described,
            "exit_code": outcome.exit_code,
            "timed_out": outcome.timed_out,
            // A queryable sibling of `timed_out` rather than only a phrase
            // inside `note`, so "did any stage leave something running?" is
            // answerable from `choco --json` without matching free text.
            "escaped": outcome.escaped,
            "duration_ms": duration_ms(outcome.duration),
            "stdout_tail": tail(&outcome.stdout),
            "stderr_tail": tail(&outcome.stderr),
        });
        if let Some(note) = note {
            payload["note"] = Value::String(note);
        }
        self.append_command_event(task_id, payload).await;

        self.finish_detached(
            DetachedKind::Shell,
            task_id,
            definition,
            stage_name,
            stage_outcome,
            captured,
        )
        .await;
    }
}
