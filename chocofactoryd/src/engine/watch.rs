use super::runners::DetachedKind;
use super::shell_stage::{describe_command, duration_ms, elapsed_ms, tail, warn_if_escaped};
use super::stage_capture::derive_capture;
use super::*;
use chocofactory_core::duration::format_duration;

/// Everything a detached poll runner needs, resolved once on stage entry.
///
/// A struct rather than eight parameters threaded through three functions:
/// the trio hands this straight down untouched, and `outcomes` in
/// particular must be compiled exactly once for the whole stage rather
/// than per attempt.
struct WatchRun {
    command: ShellCommand,
    capture: Option<Capture>,
    /// The watcher as defined: `interval` and `backoff` pick each sleep (#179).
    watch: Watch,
    /// When the stage was entered, from `payload.poll_window`; backoff steps
    /// are measured from here.
    entered_at: DateTime<Utc>,
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
/// removes it when `stage` has no watcher (or isn't defined at all). A
/// watcher is a `poll`'s loop or a `human_gate`'s `watch:` (#175); the key
/// keeps its name so tasks already parked in a poll stage still have one.
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
    let timeout = match definition.stages.get(stage).and_then(|def| def.watch()) {
        Some(watch) => watch.timeout,
        None => {
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

/// A gate that a watcher's timeout parked the task at (#179).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimedOutWatch {
    /// The watcher stage that timed out.
    pub from: String,
    pub timeout: Option<Duration>,
    /// Where a plain reply at the gate sends the task (its `on.resumed`).
    pub resumes_to: Option<String>,
}

/// Whether a task at `current_stage` is waiting there because a watcher
/// timed out, so that `retry` can watch again. Callers check the task is
/// `open`. All of these must hold: the stage is a `human_gate` with no
/// watcher of its own (a gate that already watches never gets a second
/// one); the task arrived with outcome `timeout`; it came from a stage that
/// has a watcher; and that stage's `timeout` edge leads to this gate.
pub(crate) fn timed_out_watch(
    definition: &WorkflowDefinition,
    current_stage: &str,
    payload: &Value,
) -> Option<TimedOutWatch> {
    let gate = definition.stages.get(current_stage)?;
    if !matches!(gate.kind, StageKind::HumanGate { .. }) || gate.watch().is_some() {
        return None;
    }
    let arrival = payload.get("arrival")?;
    if arrival.get("outcome")?.as_str()? != "timeout" {
        return None;
    }
    let from = arrival.get("from")?.as_str()?;
    let watcher = definition.stages.get(from)?;
    let watch = watcher.watch()?;
    if watcher.on.get("timeout").map(String::as_str) != Some(current_stage) {
        return None;
    }
    Some(TimedOutWatch {
        from: from.to_string(),
        timeout: watch.timeout,
        resumes_to: gate.on.get("resumed").cloned(),
    })
}

/// Which kind of attempt `run_watch` is about to run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    /// An ordinary attempt inside the window being watched.
    Regular,
    /// The first attempt of a catch-up, run the moment the missed deadline
    /// is found.
    CatchUpFirst,
    /// The last look, after the window (deadline or catch-up) is over.
    Final,
}

/// A bounded extra watch after a missed deadline (#313). In memory only.
#[derive(Clone, Copy)]
struct CatchUp {
    end: DateTime<Utc>,
    /// How long past the deadline the loop found it.
    late: Duration,
    len: Duration,
}

impl CatchUp {
    fn first_note(&self) -> String {
        format!(
            "the deadline passed {} ago while the stage could not watch; watching until {} UTC",
            format_duration(round_for_note(self.late)),
            self.end.format("%H:%M:%S")
        )
    }
}

/// The longest a final attempt may run: its window is gone, so it gets
/// the interval, never more than a minute.
pub(super) fn final_attempt_limit(interval: Duration) -> Duration {
    interval.min(Duration::from_secs(60))
}

/// How long a catch-up watches: the stage's own `timeout`, never more
/// than half an hour.
pub(super) fn catch_up_len(timeout: Duration) -> Duration {
    timeout.min(Duration::from_secs(30 * 60))
}

/// Whether a deadline was missed: this runner had already watched, and the
/// loop found the deadline passed by *more than* one interval.
pub(super) fn missed(attempts: u64, late: Duration, interval: Duration) -> bool {
    attempts > 0 && late > interval
}

/// Whole minutes once a duration is a minute or more, for timeline notes.
fn round_for_note(d: Duration) -> Duration {
    if d >= Duration::from_secs(60) {
        Duration::from_secs(d.as_secs() / 60 * 60)
    } else {
        d
    }
}

/// What is left of a wall-clock budget; `ZERO` once the deadline passed.
pub(super) fn remaining_budget(deadline: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (deadline - now).to_std().unwrap_or(Duration::ZERO)
}

impl WorkflowEngine {
    /// Starts a `poll` stage's loop (§5.2) and returns immediately.
    pub(super) async fn enter_poll(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageKind::Poll { capture, watch } = &entry.stage_def.kind else {
            unreachable!("enter_poll is only called for Poll stages")
        };
        self.start_watch(entry, watch, *capture).await
    }

    /// Starts a stage's watcher loop and returns immediately. Shared by a
    /// `poll` stage and a `human_gate` with a `watch:` (#175).
    ///
    /// Everything that should fail the transition that entered the stage —
    /// where a caller can still see the error — is resolved here rather
    /// than in the detached loop: the task's working directory, and the
    /// `outcomes:` patterns, which are compiled once for the whole stage
    /// instead of per attempt.
    pub(super) async fn start_watch(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
        watch: &Watch,
        capture: Option<Capture>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            payload,
            ..
        } = *entry;
        let (command, env) = self
            .render_stage_command(entry, &watch.command, &watch.env)
            .await?;
        // The deadline was computed once, on entry, and stored in the same
        // write that moved the task here (#52). A missing or malformed
        // window is an invariant violation, not a cue to invent a budget.
        let (entered_at, deadline) = match poll_window_for(payload, stage_name) {
            Ok(Some(window)) => (window.entered_at, window.deadline),
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

        let compiled =
            poll::compile(&watch.outcomes).map_err(|err| EngineError::InvalidPollPattern {
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
                watch: watch.clone(),
                entered_at,
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
        let spawned = self.spawn_registered_runner(
            &registered_task_id,
            DetachedKind::Poll,
            move |runner_id| async move {
                engine
                    .run_watch(&task_id, &definition, &stage_name, run)
                    .await;
                engine.finish_runner(&task_id, runner_id);
            },
        );
        if !spawned {
            tracing::warn!(
                task_id = %registered_task_id,
                "daemon is shutting down; poll stage not started, the next start recovers it"
            );
        }
    }

    /// Runs the command on `interval` until an outcome matches or the
    /// `timeout` budget runs out (§5.2). Serves `poll` stages and the
    /// `watch:` of a `human_gate` alike.
    ///
    /// The budget is a wall-clock deadline (#52): `run.deadline` was fixed
    /// when the stage was entered and survives a daemon restart, and every
    /// check against it reads `self.now()`, so it keeps counting while the
    /// machine sleeps. Only the `interval` sleep and each attempt's kill
    /// timer run on the monotonic clock, which stops during a sleep.
    ///
    /// **A window that is over is never reported without a last look
    /// (#313).** When the loop finds the window over (one place: the top of
    /// the loop, which the sleep and a resumed runner both reach), it runs
    /// one more attempt — the *final attempt*, capped at
    /// `final_attempt_limit` — and only then reports `timeout`. If the
    /// stored deadline was *missed* (this runner had already watched, and
    /// the deadline passed by more than one interval: the machine slept or
    /// the runner stalled), it instead watches a bounded *catch-up* of
    /// `catch_up_len`, at most once, which ends with a final attempt too.
    /// The catch-up lives in memory only: a restart or pause during it
    /// falls back to one final attempt. An attempt killed because the
    /// budget ran out was already the stage's last look, so it reports
    /// `timeout` at once.
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
        let entered_at = run.entered_at;
        let mut attempt: u64 = 0;
        // What the previous attempt produced, for the "only record what
        // changed" rule below. `None` until the first attempt reports.
        let mut previous: Option<String> = None;
        // Set once, in memory only, when the deadline was missed.
        let mut catch_up: Option<CatchUp> = None;
        // When the last attempt returned: time spent inside an attempt was
        // spent watching, so it does not count towards how late the loop is.
        let mut last_attempt_end: Option<DateTime<Utc>> = None;

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

            // The window being watched: the catch-up's end during one, else
            // the stored deadline.
            let window_end = catch_up.map(|c| c.end).or(deadline);
            let mut remaining = window_end.map(|at| remaining_budget(at, self.now()));
            let mut kind = AttemptKind::Regular;
            if let (Some(end), Some(Duration::ZERO)) = (window_end, remaining) {
                // The one place a finished window is handled.
                let now = self.now();
                let interval = self.interval_now(&run, entered_at);
                let since = last_attempt_end.map_or(end, |at| at.max(end));
                let late = (now - since).to_std().unwrap_or(Duration::ZERO);
                let started_catch_up = match run.watch.timeout {
                    Some(timeout) if catch_up.is_none() && missed(attempt, late, interval) => {
                        let len = catch_up_len(timeout);
                        let end = now
                            + chrono::Duration::from_std(len)
                                .unwrap_or_else(|_| chrono::Duration::zero());
                        Some(CatchUp { end, late, len })
                    }
                    _ => None,
                };
                match started_catch_up {
                    Some(started) => {
                        tracing::info!(
                            task_id,
                            stage = stage_name,
                            late_secs = late.as_secs(),
                            catch_up_until = %started.end.to_rfc3339(),
                            "poll deadline was missed; watching a catch-up before giving up"
                        );
                        catch_up = Some(started);
                        remaining = Some(remaining_budget(started.end, self.now()));
                        kind = AttemptKind::CatchUpFirst;
                    }
                    None => kind = AttemptKind::Final,
                }
            }

            attempt += 1;
            let started = std::time::Instant::now();
            // Each regular attempt is capped at whatever is left of the
            // window rather than at `interval`: a command that legitimately
            // takes longer than its own interval — a slow `gh` call on a
            // 30s poll — would otherwise be killed on every single attempt
            // and the stage could never resolve. The final attempt gets
            // `final_attempt_limit` instead: its window is already gone, so
            // there is no budget to cap it at, and a hung command must not
            // hold the stage open. With no `timeout:` at all there is no
            // cap, and a hung command parks the task, the same gap `shell`
            // carries without one.
            let limit = if kind == AttemptKind::Final {
                Some(final_attempt_limit(self.interval_now(&run, entered_at)))
            } else if attempt > 1
                || (self.now() - entered_at).to_std().unwrap_or_default()
                    > final_attempt_limit(self.interval_now(&run, entered_at))
            {
                // A sliver of window left (less than the final attempt's
                // cap) is too little for a regular attempt: it would be
                // killed before it could print. Give it the cap instead;
                // the window ends after it and the final attempt follows.
                // A fresh stage's first attempt (entered just now) is
                // excluded, so a `timeout:` shorter than the cap still
                // bounds it; a resumed runner's first attempt is not.
                let cap = final_attempt_limit(self.interval_now(&run, entered_at));
                remaining.map(|r| r.max(cap))
            } else {
                remaining
            };
            let outcome = match shell::run(&run.command, &run.cwd, limit, &run.env).await {
                Ok(outcome) => outcome,
                // Nothing ran and nothing will: no `sh` on PATH, or a
                // `script_file` that isn't executable. Retrying on an
                // interval would just burn the whole budget to reach the
                // same place, so this ends the poll — also on the final
                // attempt, where it is never collapsed into `timeout`.
                Err(err @ shell::ShellError::Spawn(_)) => {
                    tracing::error!(
                        task_id, stage = stage_name, %err,
                        "poll stage command could not be started"
                    );
                    let mut payload = json!({
                        "stage": stage_name,
                        "command": described,
                        "attempt": attempt,
                        "exit_code": Value::Null,
                        "timed_out": false,
                        "duration_ms": elapsed_ms(started),
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "note": err.to_string(),
                    });
                    if let (AttemptKind::CatchUpFirst, Some(c)) = (kind, catch_up) {
                        payload["catch_up_until"] = json!(c.end.to_rfc3339());
                    }
                    self.append_command_event(task_id, payload).await;
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
                // simply contributes no output to match against. On the
                // final attempt there is no next one: it ends in `timeout`
                // with the error in the note. Either way no *older*
                // attempt's output is reported under this attempt's number.
                Err(err @ shell::ShellError::Io(_)) => {
                    tracing::warn!(
                        task_id, stage = stage_name, %err,
                        "poll stage command ran but its output could not be read"
                    );
                    let note = err.to_string();
                    last_attempt_end = Some(self.now());
                    if kind == AttemptKind::Final {
                        self.finish_poll_timed_out(
                            task_id,
                            definition,
                            stage_name,
                            &described,
                            attempt,
                            None,
                            entered_at,
                            catch_up.as_ref(),
                            Some(&note),
                        )
                        .await;
                        return;
                    }
                    let mut payload = json!({
                        "stage": stage_name,
                        "command": described,
                        "attempt": attempt,
                        "exit_code": Value::Null,
                        "timed_out": false,
                        "duration_ms": elapsed_ms(started),
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "note": note,
                    });
                    // Prefixed with a NUL so an I/O failure can never
                    // collide with a command that happens to print the
                    // same text, which would suppress the event.
                    let key = format!("\0io:{note}");
                    if let (AttemptKind::CatchUpFirst, Some(c)) = (kind, catch_up) {
                        payload["note"] = json!(format!("{}; {note}", c.first_note()));
                        payload["catch_up_until"] = json!(c.end.to_rfc3339());
                        previous = Some(key);
                        self.append_command_event(task_id, payload).await;
                    } else {
                        self.record_poll_attempt(task_id, &mut previous, key, payload)
                            .await;
                    }
                    self.sleep_before_next_attempt(
                        self.interval_now(&run, entered_at),
                        catch_up.map(|c| c.end).or(deadline),
                    )
                    .await;
                    continue;
                }
            };
            last_attempt_end = Some(self.now());

            if let Some(matched) = run.outcomes.matching(&outcome.stdout) {
                let (captured, capture_note) =
                    derive_capture(run.capture, &outcome.stdout, task_id, stage_name, "stdout");
                let mut note = format!("matched \"{}\" on attempt {attempt}", matched.pattern);
                if let Some(c) = catch_up {
                    note.push_str("; in the catch-up after a missed deadline");
                    if kind == AttemptKind::CatchUpFirst {
                        note.push_str(&format!(
                            " (the deadline passed {} ago while the stage could not watch)",
                            format_duration(round_for_note(c.late))
                        ));
                    }
                }
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
                let mut payload = json!({
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
                });
                if let (AttemptKind::CatchUpFirst, Some(c)) = (kind, catch_up) {
                    payload["catch_up_until"] = json!(c.end.to_rfc3339());
                }
                self.append_command_event(task_id, payload).await;
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
            // the interval says — it was the stage's last look. The final
            // attempt is the last look by definition.
            let killed = outcome.timed_out;
            warn_if_escaped(task_id, stage_name, &outcome);

            // Reported once, by `finish_poll_timed_out`, which carries the
            // same fields plus the reason. Passing it through the recorder
            // as well would put two entries on the timeline for one attempt.
            if killed || kind == AttemptKind::Final {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    Some(&outcome),
                    entered_at,
                    catch_up.as_ref(),
                    None,
                )
                .await;
                return;
            }

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
            let key = format!("{}\0exit:{:?}", outcome.stdout.trim(), outcome.exit_code);
            let mut payload = json!({
                "stage": stage_name,
                "command": described,
                "attempt": attempt,
                "exit_code": outcome.exit_code,
                "timed_out": outcome.timed_out,
                "escaped": outcome.escaped,
                "duration_ms": duration_ms(outcome.duration),
                "stdout_tail": tail(&outcome.stdout),
                "stderr_tail": tail(&outcome.stderr),
            });
            if let (AttemptKind::CatchUpFirst, Some(c)) = (kind, catch_up) {
                // Always recorded, whatever the output: it is the one
                // entry that tells a reader the stage is catching up.
                payload["note"] = json!(c.first_note());
                payload["catch_up_until"] = json!(c.end.to_rfc3339());
                previous = Some(key);
                self.append_command_event(task_id, payload).await;
            } else {
                self.record_poll_attempt(task_id, &mut previous, key, payload)
                    .await;
            }

            self.sleep_before_next_attempt(
                self.interval_now(&run, entered_at),
                catch_up.map(|c| c.end).or(deadline),
            )
            .await;
        }
    }

    /// The interval to sleep now: the backoff step for how long the stage
    /// has been running. A clock set backwards gives the base interval.
    fn interval_now(&self, run: &WatchRun, entered_at: DateTime<Utc>) -> Duration {
        let elapsed = (self.now() - entered_at).to_std().unwrap_or(Duration::ZERO);
        run.watch.interval_at(elapsed)
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

    /// Waits out the interval, or what is left of the window if shorter.
    ///
    /// Measured from the end of one attempt to the start of the next
    /// rather than on a fixed cadence, so a command slower than its own
    /// interval can't have attempts overlap and stack up on top of each
    /// other in the task's working copy.
    ///
    /// `deadline` is the window's end, wall-clock (#52), compared against
    /// `self.now()`; the sleep itself is `interval.min(remaining)` on
    /// tokio's monotonic clock, so after the machine wakes an expired
    /// window is noticed within one interval.
    async fn sleep_before_next_attempt(&self, interval: Duration, deadline: Option<DateTime<Utc>>) {
        let Some(deadline) = deadline else {
            tokio::time::sleep(interval).await;
            return;
        };
        let remaining = remaining_budget(deadline, self.now());
        if remaining.is_zero() {
            return;
        }
        // Never sleep past the deadline: a 60s interval under a 70s budget
        // should give up at 70s, not at 120s.
        tokio::time::sleep(interval.min(remaining)).await;
    }

    /// Ends a poll whose window is over with no matching outcome (§5.2's
    /// `on_timeout`), after its last look. The loader guarantees a
    /// `timeout` edge exists whenever the stage sets a `timeout:`, so this
    /// reaches `finish_detached`'s park path only for a hand-built
    /// definition. `attempts` includes the final attempt; `last` is that
    /// attempt (or the killed one), `None` when its output could not be
    /// read, in which case `io_note` carries the error.
    #[allow(clippy::too_many_arguments)]
    async fn finish_poll_timed_out(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        described: &str,
        attempts: u64,
        last: Option<&shell::ShellOutcome>,
        entered_at: DateTime<Utc>,
        catch_up: Option<&CatchUp>,
        io_note: Option<&str>,
    ) {
        tracing::info!(
            task_id,
            stage = stage_name,
            attempts,
            "poll stage gave up: its timeout elapsed with no matching outcome"
        );

        let elapsed = round_for_note((self.now() - entered_at).to_std().unwrap_or(Duration::ZERO));
        let mut note = match catch_up {
            None => format!(
                "no outcome matched in {attempts} attempts over {}; timeout elapsed",
                format_duration(elapsed)
            ),
            Some(c) => format!(
                "no outcome matched in {attempts} attempts over {}; the deadline was missed by {}, so the stage watched {} more",
                format_duration(elapsed),
                format_duration(round_for_note(c.late)),
                format_duration(round_for_note(c.len)),
            ),
        };
        if let Some(io_note) = io_note {
            note.push_str(&format!(
                "; the final attempt's output could not be read: {io_note}"
            ));
        }
        // Outranks the plain timeout wording: the workflow is about to
        // follow its `timeout` edge while the last command may still be
        // running in the same working copy.
        if last.is_some_and(|outcome| outcome.escaped) {
            note = "could not confirm the process group exited — something may still be running"
                .to_string();
        }
        let mut payload = json!({
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
        });
        if let Some(c) = catch_up {
            payload["catch_up_until"] = json!(c.end.to_rfc3339());
        }
        self.append_command_event(task_id, payload).await;

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
