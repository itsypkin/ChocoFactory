use super::runners::DetachedKind;
use super::shell_stage::{describe_command, duration_ms, elapsed_ms, tail, warn_if_escaped};
use super::stage_capture::derive_capture;
use super::*;

/// Everything a detached poll runner needs, resolved once on stage entry.
///
/// A struct rather than eight parameters threaded through three functions:
/// the trio hands this straight down untouched, and `outcomes` in
/// particular must be compiled exactly once for the whole stage rather
/// than per attempt.
struct WatchRun {
    command: ShellCommand,
    capture: Option<Capture>,
    interval: Duration,
    /// Wall-clock deadline (#52), read from `payload.poll_window`; `None`
    /// when the stage has no `timeout:`.
    deadline: Option<DateTime<Utc>>,
    outcomes: poll::CompiledOutcomes,
    cwd: PathBuf,
    /// Rendered once on stage entry (#101) and passed to every attempt.
    env: Vec<(String, String)>,
}

/// The persisted `payload.poll_window` (#52): when the task entered a
/// `poll` stage and the wall-clock instant its `timeout:` runs out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct PollWindow {
    pub(super) stage: String,
    pub(super) entered_at: DateTime<Utc>,
    pub(super) deadline: Option<DateTime<Utc>>,
}

/// Stamps `payload.poll_window` for a task entering `stage` (#52), or
/// removes it when `stage` isn't a `poll` (or isn't defined at all).
///
/// Pure, and **the only place a deadline is computed**. The window is a
/// sibling of `payload.stages`/`payload.task`, never under
/// `payload.stages.<stage>`: that key is the capture slot, and a capture
/// replaces it wholesale.
pub(super) fn set_poll_window(
    payload: &mut Value,
    definition: &WorkflowDefinition,
    stage: &str,
    now: DateTime<Utc>,
) -> Result<(), EngineError> {
    // Same non-object handling as `merge_stage_capture`.
    if !payload.is_object() {
        *payload = json!({});
    }
    let Some(object) = payload.as_object_mut() else {
        return Ok(());
    };
    let timeout = match definition.stages.get(stage).map(|def| &def.kind) {
        Some(StageKind::Poll { timeout, .. }) => *timeout,
        _ => {
            object.remove("poll_window");
            return Ok(());
        }
    };
    let deadline = match timeout {
        None => Value::Null,
        Some(limit) => {
            let overflow = |reason: String| EngineError::PollWindow {
                stage: stage.to_string(),
                reason,
            };
            let limit = chrono::Duration::from_std(limit)
                .map_err(|err| overflow(format!("timeout is out of range: {err}")))?;
            let at = now
                .checked_add_signed(limit)
                .ok_or_else(|| overflow("deadline overflows the calendar".to_string()))?;
            json!(at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        }
    };
    object.insert(
        "poll_window".to_string(),
        json!({
            "stage": stage,
            "entered_at": now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            "deadline": deadline,
        }),
    );
    Ok(())
}

/// Reads `payload.poll_window` for `current_stage`. `Ok(None)` when the
/// window is absent or belongs to another stage; `Err` when it is malformed.
pub(super) fn poll_window_for(
    payload: &Value,
    current_stage: &str,
) -> Result<Option<PollWindow>, String> {
    let Some(raw) = payload.get("poll_window") else {
        return Ok(None);
    };
    let window: PollWindow = serde_json::from_value(raw.clone())
        .map_err(|err| format!("malformed poll_window: {err}"))?;
    if window.stage != current_stage {
        return Ok(None);
    }
    Ok(Some(window))
}

/// What is left of a wall-clock budget; `ZERO` once the deadline passed.
pub(super) fn remaining_budget(deadline: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (deadline - now).to_std().unwrap_or(Duration::ZERO)
}

impl WorkflowEngine {
    /// Starts a `poll` stage's loop (§5.2) and returns immediately.
    ///
    /// Everything that should fail the transition that entered the stage —
    /// where a caller can still see the error — is resolved here rather
    /// than in the detached loop: the task's working directory, and the
    /// `outcomes:` patterns, which are compiled once for the whole stage
    /// instead of per attempt.
    pub(super) async fn enter_poll(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            payload,
            ..
        } = *entry;
        let StageKind::Poll {
            command,
            capture,
            interval,
            timeout: _,
            outcomes,
            env,
        } = &entry.stage_def.kind
        else {
            unreachable!("enter_poll is only called for Poll stages")
        };
        let capture = *capture;
        let interval = *interval;
        let (command, env) = self.render_stage_command(entry, command, env).await?;
        // The deadline was computed once, on entry, and stored in the same
        // write that moved the task here (#52). A missing or malformed
        // window is an invariant violation, not a cue to invent a budget.
        let deadline = match poll_window_for(payload, stage_name) {
            Ok(Some(window)) => window.deadline,
            Ok(None) => {
                return Err(EngineError::PollWindow {
                    stage: stage_name.to_string(),
                    reason: "no poll_window recorded for this stage".to_string(),
                });
            }
            Err(reason) => {
                return Err(EngineError::PollWindow {
                    stage: stage_name.to_string(),
                    reason,
                });
            }
        };
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;

        let compiled = poll::compile(outcomes).map_err(|err| EngineError::InvalidPollPattern {
            stage: stage_name.to_string(),
            reason: err.to_string(),
        })?;
        let cwd = working_dir(&task, definition)?;

        self.spawn_poll_runner(
            task_id.to_string(),
            Arc::clone(definition),
            stage_name.to_string(),
            WatchRun {
                command,
                capture,
                interval,
                deadline,
                outcomes: compiled,
                cwd,
                env,
            },
        );
        Ok(())
    }

    /// Deliberately a *synchronous* fn, for the same reason
    /// `spawn_shell_runner` is: the spawned future eventually calls
    /// `advance` → `enter_stage` → `enter_poll`, i.e. back to here, and
    /// discharging `tokio::spawn`'s `Send` obligation from inside an
    /// `async fn` would make that cycle part of the compiler's auto-trait
    /// inference for `enter_poll`'s own future and fail to resolve.
    fn spawn_poll_runner(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        run: WatchRun,
    ) {
        let engine = Arc::clone(self);
        // Same registration as `spawn_shell_runner`, and more load-bearing
        // here: a `poll` holds its window open for minutes or hours, so
        // without this a cancelled task keeps firing its command every
        // interval until the deadline. The advisory `is_cancelled` check in
        // `run_watch` only fires *between* attempts; this stops one
        // already in flight.
        let registered_task_id = task_id.clone();
        let spawned =
            self.spawn_registered_runner(&registered_task_id, move |runner_id| async move {
                engine
                    .run_watch(&task_id, &definition, &stage_name, run)
                    .await;
                engine.finish_runner(&task_id, runner_id);
            });
        if !spawned {
            tracing::warn!(
                task_id = %registered_task_id,
                "daemon is shutting down; poll stage not started, the next start recovers it"
            );
        }
    }

    /// Runs the command on `interval` until an outcome matches or the
    /// `timeout` budget runs out (§5.2).
    ///
    /// The budget is a wall-clock deadline (#52): `run.deadline` was fixed
    /// when the stage was entered and survives a daemon restart, and every
    /// check against it reads `self.now()`, so it keeps counting while the
    /// machine sleeps. Only the `interval` sleep and each attempt's kill
    /// timer run on the monotonic clock.
    ///
    /// Unlike `shell`, the command's *exit code decides nothing*: a polled
    /// command failing is ordinary — `gh` on a rate limit or a dropped
    /// connection — and is exactly the condition polling exists to ride
    /// out. Only the output is matched. The one failure that does end the
    /// loop is a command that could not be started at all, which no amount
    /// of retrying will fix.
    async fn run_watch(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        run: WatchRun,
    ) {
        let described = describe_command(&run.command);
        // Fixed on stage entry, not here: this runner may be a resumed one.
        let deadline = run.deadline;
        let mut attempt: u64 = 0;
        // What the previous attempt produced, for the "only record what
        // changed" rule below. `None` until the first attempt reports.
        let mut previous: Option<String> = None;
        // The most recent attempt that actually ran, so every timeout path
        // can report what the command last said rather than an empty entry.
        let mut last_outcome: Option<shell::ShellOutcome> = None;

        loop {
            // A poll is the one stage kind that holds its window open for
            // minutes or hours, so unlike `shell` it cannot assume the task
            // is still where it left it. Without this, a poll with no
            // `timeout:` on a task a human has since closed would run
            // forever. This is advisory only — the authoritative check is
            // `advance_from_stage`'s `expected_stage`, taken inside the
            // per-task lock; this just stops the loop early rather than
            // letting it burn a command every interval until the deadline.
            if attempt > 0 && !self.still_in_stage(task_id, stage_name).await {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    attempts = attempt,
                    "abandoned a poll: the task had already left that stage"
                );
                return;
            }

            // Cancel needs its own check here, and can't ride on
            // `still_in_stage` above: cancelling deliberately leaves
            // `current_stage` where it was, so a cancelled poll is still
            // "in its stage" and would keep firing its command every
            // interval — an hour of `gh pr checks` on a task the operator
            // already stopped. Advisory, exactly like the check above; the
            // authoritative refusal is in `advance_from_stage`.
            if attempt > 0 && self.is_cancelled(task_id).await {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    attempts = attempt,
                    "abandoned a poll: the task was cancelled"
                );
                return;
            }

            let remaining = deadline.map(|at| remaining_budget(at, self.now()));
            if remaining == Some(Duration::ZERO) {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
                )
                .await;
                return;
            }

            attempt += 1;
            let started = std::time::Instant::now();
            // Each attempt is capped at whatever is left of the stage's
            // budget rather than at `interval`: a command that legitimately
            // takes longer than its own interval — a slow `gh` call on a
            // 30s poll — would otherwise be killed on every single attempt
            // and the stage could never resolve. With no `timeout:` at all
            // there is no cap, and a hung command parks the task, the same
            // gap `shell` carries without one.
            let outcome = match shell::run(&run.command, &run.cwd, remaining, &run.env).await {
                Ok(outcome) => outcome,
                // Nothing ran and nothing will: no `sh` on PATH, or a
                // `script_file` that isn't executable. Retrying on an
                // interval would just burn the whole budget to reach the
                // same place, so this ends the poll.
                Err(err @ shell::ShellError::Spawn(_)) => {
                    tracing::error!(
                        task_id, stage = stage_name, %err,
                        "poll stage command could not be started"
                    );
                    self.append_command_event(
                        task_id,
                        json!({
                            "stage": stage_name,
                            "command": described,
                            "attempt": attempt,
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
                        DetachedKind::Poll,
                        task_id,
                        definition,
                        stage_name,
                        "error",
                        None,
                    )
                    .await;
                    return;
                }
                // The command *did* run — possibly for a long time — and
                // only reading its pipes failed. That says nothing about
                // the state being polled, so keep polling; the attempt
                // simply contributes no output to match against.
                Err(err @ shell::ShellError::Io(_)) => {
                    tracing::warn!(
                        task_id, stage = stage_name, %err,
                        "poll stage command ran but its output could not be read"
                    );
                    let note = err.to_string();
                    self.record_poll_attempt(
                        task_id,
                        &mut previous,
                        // Prefixed with a NUL so an I/O failure can never
                        // collide with a command that happens to print the
                        // same text, which would suppress the event.
                        format!("\0io:{note}"),
                        json!({
                            "stage": stage_name,
                            "command": described,
                            "attempt": attempt,
                            "exit_code": Value::Null,
                            "timed_out": false,
                            "duration_ms": elapsed_ms(started),
                            "stdout_tail": "",
                            "stderr_tail": "",
                            "note": note,
                        }),
                    )
                    .await;
                    // This attempt produced no outcome, and an *older* one
                    // must not be reported under this attempt's number — a
                    // timeout entry saying "attempt 7" while carrying
                    // attempt 6's exit code and output would be a quietly
                    // wrong record. Clearing it means both timeout paths
                    // below report empty fields after an I/O failure, which
                    // is honest about what the last attempt actually
                    // yielded: nothing.
                    last_outcome = None;
                    if self
                        .sleep_before_next_attempt(run.interval, deadline)
                        .await
                        .is_break()
                    {
                        self.finish_poll_timed_out(
                            task_id,
                            definition,
                            stage_name,
                            &described,
                            attempt,
                            last_outcome.as_ref(),
                        )
                        .await;
                        return;
                    }
                    continue;
                }
            };

            if let Some(matched) = run.outcomes.matching(&outcome.stdout) {
                let (captured, capture_note) =
                    derive_capture(run.capture, &outcome.stdout, task_id, stage_name, "stdout");
                let mut note = format!("matched \"{}\" on attempt {attempt}", matched.pattern);
                if let Some(capture_note) = capture_note {
                    note.push_str("; ");
                    note.push_str(&capture_note);
                }
                // A match can still come from an attempt that was killed at
                // the budget's edge — the output it printed before dying is
                // real and worth honouring — but if its process group
                // outlived the kill, that outranks everything else here:
                // the workflow is about to move on while something may
                // still be running in the same working copy. Same
                // precedence `run_shell_stage` gives it.
                if outcome.escaped {
                    note = "could not confirm the process group exited — something may still be running"
                        .to_string();
                }
                warn_if_escaped(task_id, stage_name, &outcome);
                // Always recorded, changed output or not: this is the
                // attempt that decided the stage.
                self.append_command_event(
                    task_id,
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "attempt": attempt,
                        "exit_code": outcome.exit_code,
                        "timed_out": outcome.timed_out,
                        "escaped": outcome.escaped,
                        "duration_ms": duration_ms(outcome.duration),
                        // Queryable siblings of the note, so "which rule
                        // fired?" is answerable from `choco --json`
                        // without matching free text.
                        "matched": matched.pattern,
                        "outcome": matched.then,
                        "stdout_tail": tail(&outcome.stdout),
                        "stderr_tail": tail(&outcome.stderr),
                        "note": note,
                    }),
                )
                .await;
                self.finish_detached(
                    DetachedKind::Poll,
                    task_id,
                    definition,
                    stage_name,
                    matched.then,
                    captured,
                )
                .await;
                return;
            }

            // No match. A killed attempt means the budget it was capped at
            // is now spent, so the stage is out of time regardless of what
            // the interval says.
            let killed = outcome.timed_out;
            warn_if_escaped(task_id, stage_name, &outcome);

            // A killed attempt is reported once, by `finish_poll_timed_out`
            // below, which carries the same fields plus the reason. Passing
            // it through here as well would put two entries on the timeline
            // for one attempt.
            if !killed {
                self.record_poll_attempt(
                    task_id,
                    &mut previous,
                    // Compared trimmed, matching what `tail` puts on the
                    // timeline: two attempts differing only in trailing
                    // whitespace would otherwise record two entries a reader
                    // can't tell apart.
                    //
                    // Keyed on the exit code as well as the output. A poll
                    // whose command prints nothing on success — #78's
                    // verdict poll is exactly that, empty until someone
                    // reviews — makes "no verdict yet" and "`gh` has been
                    // failing for an hour" the same empty string, so on
                    // output alone the failure records nothing after the
                    // first attempt and the stage looks like patient
                    // waiting right up to its timeout. The exit code is
                    // what tells them apart.
                    format!("{}\0exit:{:?}", outcome.stdout.trim(), outcome.exit_code),
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "attempt": attempt,
                        "exit_code": outcome.exit_code,
                        "timed_out": outcome.timed_out,
                        "escaped": outcome.escaped,
                        "duration_ms": duration_ms(outcome.duration),
                        "stdout_tail": tail(&outcome.stdout),
                        "stderr_tail": tail(&outcome.stderr),
                    }),
                )
                .await;
            }

            // Kept so whichever timeout path fires can report what the
            // command last actually said. Without it, a poll that printed
            // `PENDING` for an hour and then ran out of budget leaves a
            // final timeline entry showing nothing at all — the repeated
            // attempts having been deliberately suppressed above.
            last_outcome = Some(outcome);

            if killed {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
                )
                .await;
                return;
            }

            if self
                .sleep_before_next_attempt(run.interval, deadline)
                .await
                .is_break()
            {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
                )
                .await;
                return;
            }
        }
    }

    /// Whether the task is still sitting in the stage this runner belongs
    /// to.
    ///
    /// A read failure answers "yes" on purpose: the alternative is
    /// abandoning a live poll because one `SELECT` failed, which strands a
    /// task nothing will come back to. The error is logged rather than
    /// dropped, and a genuinely departed stage is caught anyway by
    /// `advance_from_stage`'s `expected_stage` check when the poll
    /// eventually reports.
    async fn still_in_stage(&self, task_id: &str, stage_name: &str) -> bool {
        match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => state.current_stage == stage_name,
            // No row at all means the task was deleted underneath us;
            // there is nothing left to poll for.
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, stage = stage_name, %err,
                    "could not confirm a polling task is still in its stage; continuing to poll"
                );
                true
            }
        }
    }

    /// Records an attempt that decided nothing, but only when it said
    /// something new.
    ///
    /// A `gh pr checks` poll at 30s over an hour is 120 attempts printing
    /// the same `PENDING`; one timeline entry per attempt would bury every
    /// other event the task produced, and the retention job prunes by age
    /// alone so nothing else bounds it. Recording only what *changed*
    /// keeps the useful signal — the moment the output flips — while
    /// collapsing the noise, and the first attempt always reports because
    /// it has nothing to be the same as.
    ///
    /// "Changed" is whatever key the caller passes, not the output alone —
    /// `run_watch` folds the exit code in, so a command that starts
    /// failing without changing what it prints is still a change worth a
    /// timeline entry.
    async fn record_poll_attempt(
        &self,
        task_id: &str,
        previous: &mut Option<String>,
        current: String,
        payload: Value,
    ) {
        let changed = previous.as_deref() != Some(current.as_str());
        *previous = Some(current);
        if changed {
            self.append_command_event(task_id, payload).await;
        }
    }

    /// Waits out the interval, or reports that the budget is gone.
    ///
    /// Measured from the end of one attempt to the start of the next
    /// rather than on a fixed cadence, so a command slower than its own
    /// interval can't have attempts overlap and stack up on top of each
    /// other in the task's working copy.
    ///
    /// `deadline` is wall-clock (#52) and is compared against `self.now()`;
    /// the sleep itself is `interval.min(remaining)` on tokio's monotonic
    /// clock, so after the machine wakes an expired deadline is noticed
    /// within one interval.
    async fn sleep_before_next_attempt(
        &self,
        interval: Duration,
        deadline: Option<DateTime<Utc>>,
    ) -> std::ops::ControlFlow<()> {
        let Some(deadline) = deadline else {
            tokio::time::sleep(interval).await;
            return std::ops::ControlFlow::Continue(());
        };
        let remaining = remaining_budget(deadline, self.now());
        if remaining.is_zero() {
            return std::ops::ControlFlow::Break(());
        }
        // Never sleep past the deadline: a 60s interval under a 70s budget
        // should give up at 70s, not at 120s.
        tokio::time::sleep(interval.min(remaining)).await;
        std::ops::ControlFlow::Continue(())
    }

    /// Ends a poll that ran out of budget with no matching outcome (§5.2's
    /// `on_timeout`). The loader guarantees a `timeout` edge exists
    /// whenever the stage sets a `timeout:`, so this reaches
    /// `finish_detached`'s park path only for a hand-built definition.
    async fn finish_poll_timed_out(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        described: &str,
        attempts: u64,
        last: Option<&shell::ShellOutcome>,
    ) {
        tracing::info!(
            task_id,
            stage = stage_name,
            attempts,
            "poll stage gave up: its timeout elapsed with no matching outcome"
        );

        let mut note = if attempts == 0 {
            "timeout elapsed before the first attempt: the deadline passed while the daemon was down"
                .to_string()
        } else {
            format!("no outcome matched in {attempts} attempts; timeout elapsed")
        };
        // Outranks the plain timeout wording: the workflow is about to
        // follow its `timeout` edge while the last command may still be
        // running in the same working copy.
        if last.is_some_and(|outcome| outcome.escaped) {
            note = "could not confirm the process group exited — something may still be running"
                .to_string();
        }
        self.append_command_event(
            task_id,
            json!({
                "stage": stage_name,
                "command": described,
                "attempt": attempts,
                "exit_code": last.and_then(|outcome| outcome.exit_code),
                "timed_out": true,
                "escaped": last.is_some_and(|outcome| outcome.escaped),
                "duration_ms": last.map_or(0, |outcome| duration_ms(outcome.duration)),
                "stdout_tail": last.map_or_else(String::new, |outcome| tail(&outcome.stdout)),
                "stderr_tail": last.map_or_else(String::new, |outcome| tail(&outcome.stderr)),
                "note": note,
            }),
        )
        .await;

        self.finish_detached(
            DetachedKind::Poll,
            task_id,
            definition,
            stage_name,
            "timeout",
            None,
        )
        .await;
    }
}
