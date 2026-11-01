//! Pausing and resuming a task (#295).
//!
//! `pause` moves an `open` task to `pausing`, stops its watchers (and, with
//! `--now`, its running turns and commands), and lets [`settle_pause`] make it
//! `paused` once nothing of its own is running and every ended turn's result
//! is applied. `resume` writes `open` and
//! then starts whatever was held. Every status change is one conditional
//! `UPDATE` under the task's lock; `dispatch_stage` is the single place a
//! stage is held.
//!
//! [`settle_pause`]: WorkflowEngine::settle_pause

use chocofactory_core::models::{PauseOutcome, ResumeOutcome, WorkflowState};

use super::parallel::{BranchEntry, BranchWork, block_for, branch_slot_state};
use super::runners::DetachedKind;
use super::turn::ReadOnlyVerdict;
use super::*;
use crate::db::{events as db_events, turn_queue};
use crate::session::StopReason;
use crate::workflow_def::Branch;

/// Errors from [`WorkflowEngine::pause_task`].
#[derive(Debug)]
pub enum PauseTaskError {
    NoSuchTask,
    /// The task's status does not allow a pause. Carries the sentence that
    /// names the status.
    Refused(String),
    Db(sqlx::Error),
}

impl fmt::Display for PauseTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PauseTaskError::NoSuchTask => write!(f, "no such task"),
            PauseTaskError::Refused(why) => write!(f, "{why}"),
            PauseTaskError::Db(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PauseTaskError {}

impl From<sqlx::Error> for PauseTaskError {
    fn from(err: sqlx::Error) -> Self {
        PauseTaskError::Db(err)
    }
}

/// Errors from [`WorkflowEngine::resume_task`].
#[derive(Debug)]
pub enum ResumeTaskError {
    NoSuchTask,
    /// The task is not paused. Carries the sentence that names its status.
    Refused(String),
    NoWorkflowState,
    UnknownStage(String),
    Load(LoadTaskWorkflowError),
    Db(sqlx::Error),
    /// Starting what was held failed; the task has been marked stuck.
    Enter(EngineError),
    /// The stage could not be resumed and marking the task stuck failed too;
    /// the task keeps its status.
    StuckWriteFailed(String),
    /// Starting what was held failed after the task was already `open`, and
    /// the task was not marked stuck either; the message says why and what
    /// status the task has now.
    StartFailedNotStuck(String),
}

impl fmt::Display for ResumeTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResumeTaskError::NoSuchTask => write!(f, "no such task"),
            ResumeTaskError::Refused(why) => write!(f, "{why}"),
            ResumeTaskError::NoWorkflowState => write!(f, "task has no workflow_state row"),
            ResumeTaskError::UnknownStage(stage) => {
                write!(f, "workflow_state references unknown stage '{stage}'")
            }
            ResumeTaskError::Load(err) => write!(f, "{err}"),
            ResumeTaskError::Db(err) => write!(f, "{err}"),
            ResumeTaskError::Enter(err) => write!(f, "resume failed: {err}"),
            ResumeTaskError::StuckWriteFailed(reason) => write!(
                f,
                "{reason}; marking the task stuck failed, so it keeps its status"
            ),
            ResumeTaskError::StartFailedNotStuck(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for ResumeTaskError {}

impl From<sqlx::Error> for ResumeTaskError {
    fn from(err: sqlx::Error) -> Self {
        ResumeTaskError::Db(err)
    }
}

impl From<LoadTaskWorkflowError> for ResumeTaskError {
    fn from(err: LoadTaskWorkflowError) -> Self {
        ResumeTaskError::Load(err)
    }
}

/// The tail of a `resume` failure that found no work to start or wait for.
const NOTHING_MOVES: &str = "nothing is running or queued to move the task on";

fn reason_text(session: &Session) -> String {
    session
        .end_reason
        .map_or_else(|| "none".to_string(), |r| r.to_string())
}

/// Whether `session` ended in a failure its watcher would have parked, from
/// the row alone, with the text to show. Mirrors the order of checks in
/// `spawn_turn_watcher`: `cancelled` exits quietly; `paused` and
/// `daemon_stopped` are requested ends; `reaped` and `interrupted` are
/// parked on any status, as are the two `claude_settings_*` ends (#346);
/// otherwise `idle` is a completed run and `exited` is
/// a failure, with or without a reason.
pub(super) fn run_failure(session: &Session) -> Option<String> {
    let failed = match (session.status, session.end_reason) {
        (SessionStatus::Active, _) => false,
        (
            _,
            Some(
                SessionEndReason::Cancelled
                | SessionEndReason::Paused
                | SessionEndReason::DaemonStopped,
            ),
        ) => false,
        (
            _,
            Some(
                SessionEndReason::Reaped
                | SessionEndReason::Interrupted
                | SessionEndReason::ClaudeSettingsRejected
                | SessionEndReason::ClaudeSettingsUnconfirmed,
            ),
        ) => true,
        (SessionStatus::Idle, _) => false,
        (SessionStatus::Exited, _) => true,
    };
    failed.then(|| {
        format!(
            "its session ended: {}, end reason {}",
            session.status,
            reason_text(session)
        )
    })
}

/// What `resume` starts, decided from reads only.
enum ResumePlan {
    /// Nothing is held, and what exists is running, queued or waiting for a
    /// person already. At an agent or group stage `plan_resume` returns this
    /// only when that holds; otherwise it returns `Failed`.
    Nothing,
    /// Re-enter the current stage, resuming `resume` when given.
    Stage(Option<ResumeSession>),
    /// The group was entered while held: enter it now.
    EnterGroup,
    /// The stage's current-entry run ended in a failure: starting nothing
    /// would leave the task `open` with no work, so `resume` marks it stuck.
    Failed(String),
    /// Restart these unsettled branches.
    Branches(Vec<(String, Option<ResumeSession>)>),
}

/// Moves `entered_at` and `deadline` of the poll window of `stage` forward by
/// `delta`, so the time used before the pause still counts and the time spent
/// paused does not. A window of another stage, or none, is left alone.
fn shift_poll_window(payload: &mut Value, stage: &str, delta: chrono::Duration) {
    let Ok(Some(window)) = watch::poll_window_for(payload, stage) else {
        return;
    };
    let fmt = |at: DateTime<Utc>| json!(at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
    let deadline = window
        .deadline
        .map_or(Value::Null, |deadline| fmt(deadline + delta));
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "poll_window".to_string(),
            json!({
                "stage": stage,
                "entered_at": fmt(window.entered_at + delta),
                "deadline": deadline,
            }),
        );
    }
}

impl WorkflowEngine {
    /// Best-effort task-scoped timeline entry, after the commit it describes.
    pub(super) async fn append_task_event(
        &self,
        task_id: &str,
        event_type: EventType,
        payload: Value,
    ) {
        match db_events::append_for_task(&self.pool, task_id, event_type, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, %err, event = %event_type,
                "failed to record a pause event"
            ),
        }
    }

    // -----------------------------------------------------------------
    // pause
    // -----------------------------------------------------------------

    /// Pauses `task_id`: see the module docs. `now` also stops running turns
    /// and commands instead of letting them finish.
    pub async fn pause_task(
        self: &Arc<Self>,
        task_id: &str,
        now: bool,
    ) -> Result<PauseOutcome, PauseTaskError> {
        let lock = self.lock_for_task(task_id).await;
        let result = {
            let _guard = lock.lock().await;
            self.pause_task_locked(task_id, now).await
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    async fn pause_task_locked(
        self: &Arc<Self>,
        task_id: &str,
        now: bool,
    ) -> Result<PauseOutcome, PauseTaskError> {
        // Step 1 — read.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(PauseTaskError::NoSuchTask)?;
        let refuse = |why: String| Err(PauseTaskError::Refused(why));
        match (task.status.as_str(), now) {
            (TASK_STATUS_OPEN, _) | (TASK_STATUS_PAUSING, true) => {}
            (TASK_STATUS_PAUSED, _) => return refuse("task is already paused".to_string()),
            (TASK_STATUS_PAUSING, false) => {
                return refuse(
                    "task is already pausing; use --now to stop what is running".to_string(),
                );
            }
            (TASK_STATUS_STUCK, _) => {
                return refuse(format!(
                    "task is stuck: stuck tasks don't run; run 'choco task retry {task_id}' or \
                     'choco task cancel {task_id}'"
                ));
            }
            (TASK_STATUS_CLOSED, _) => return refuse("the task is closed".to_string()),
            (TASK_STATUS_CANCELLED, _) => return refuse("the task is cancelled".to_string()),
            (other, _) => return refuse(format!("task is '{other}' and cannot be paused")),
        }

        // Step 2 — write. A failure returns having stopped nothing.
        if task.status == TASK_STATUS_OPEN
            && !tasks::mark_pausing(&self.pool, task_id, self.now()).await?
        {
            let status = tasks::get(&self.pool, task_id)
                .await?
                .map_or_else(|| "gone".to_string(), |t| t.status);
            return refuse(format!("task is '{status}' and cannot be paused"));
        }

        // Step 3 — stop. Nothing here can fail the pause: a failure is
        // logged and listed in the event, and that work ends on its own.
        let mut stop_failures: Vec<String> = Vec::new();
        let state = match workflow_state::get(&self.pool, task_id).await {
            Ok(state) => state,
            Err(err) => {
                tracing::error!(task_id, %err, "could not read the stage of a task being paused");
                if now {
                    stop_failures.push(format!(
                        "could not read the task's stage, so no running turn was stopped: {err}"
                    ));
                }
                None
            }
        };
        let stage = state
            .as_ref()
            .map(|s| s.current_stage.clone())
            .unwrap_or_default();
        let shell_runners = self.detached_runner_count(task_id, DetachedKind::Shell);
        let running = self.turn_gate.running_for_task(task_id) + shell_runners;
        let kinds: &[DetachedKind] = if now {
            &[DetachedKind::Poll, DetachedKind::Shell]
        } else {
            &[DetachedKind::Poll]
        };
        self.abort_detached_runners_of(task_id, kinds).await;
        if now && state.is_some() {
            match self.live_turn_sessions(&task, state.as_ref()).await {
                Ok(live) => {
                    for (label, session_id) in live {
                        if let Err(err) = self
                            .session_manager
                            .stop(&session_id, StopReason::Paused)
                            .await
                        {
                            tracing::error!(
                                task_id, session_id, %err,
                                "could not stop a running turn for a pause; it ends on its own"
                            );
                            stop_failures.push(format!("{label}: {err}"));
                        }
                    }
                }
                Err(why) => {
                    tracing::error!(task_id, why, "could not find the turns to stop for a pause");
                    stop_failures.push(why);
                }
            }
        }

        // Step 4 — the event describes the write above, so it comes before
        // the `task_paused` that `settle_pause_locked` may write.
        self.append_task_event(
            task_id,
            EventType::TaskPausing,
            json!({
                "mode": if now { "now" } else { "graceful" },
                "stage": stage,
                "running": running,
                "stop_failures": stop_failures,
            }),
        )
        .await;
        self.settle_pause_locked(task_id).await;

        let status = tasks::get(&self.pool, task_id)
            .await?
            .map_or_else(|| TASK_STATUS_PAUSING.to_string(), |t| t.status);
        Ok(PauseOutcome {
            status,
            mode: if now { "now" } else { "graceful" }.to_string(),
            stage,
            running: u32::try_from(running).unwrap_or(u32::MAX),
            stop_failures,
        })
    }

    /// The live single-shot sessions of the task's current stage (or of each
    /// running branch of its group), as `(label, session id)`. A standing
    /// chat session is never listed: a pause does not stop it.
    async fn live_turn_sessions(
        &self,
        task: &Task,
        state: Option<&WorkflowState>,
    ) -> Result<Vec<(String, String)>, String> {
        let Some(state) = state else {
            return Ok(Vec::new());
        };
        let definition = self
            .load_task_workflow(task)
            .await
            .map_err(|err| format!("the workflow could not be loaded to find the turns: {err}"))?;
        let stage = state.current_stage.as_str();
        let Some(stage_def) = definition.stages.get(stage) else {
            return Ok(Vec::new());
        };
        let read_err = |what: &str, err: sqlx::Error| format!("{what}: {err}");
        let mut live = Vec::new();
        match &stage_def.kind {
            StageKind::AgentTurn { .. } if !stage_def.on.is_empty() => {
                let current = sessions::get_current_for_stage(&self.pool, &task.id, stage)
                    .await
                    .map_err(|err| read_err(stage, err))?;
                if let Some(session) = current
                    && session.status == SessionStatus::Active
                {
                    live.push((stage.to_string(), session.id));
                }
            }
            StageKind::Parallel { branches } => {
                let entry_no = block_for(&state.payload, stage)
                    .and_then(|block| block.get("entry"))
                    .and_then(Value::as_i64);
                if let Some(entry_no) = entry_no {
                    for name in branches.keys() {
                        if branch_slot_state(&state.payload, name) != Some("running") {
                            continue;
                        }
                        let work = self
                            .branch_progress(&task.id, name, entry_no)
                            .await
                            .map_err(|err| read_err(name, err))?;
                        if let BranchWork::Live(session) = work {
                            live.push((name.clone(), session.id));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(live)
    }

    /// Makes a `pausing` task `paused` when nothing of its own is running or
    /// unapplied: no running or settling turn and no detached runner. Takes
    /// the task lock.
    pub(super) async fn settle_pause(&self, task_id: &str) {
        let lock = self.lock_for_task(task_id).await;
        {
            let _guard = lock.lock().await;
            self.settle_pause_locked(task_id).await;
        }
        self.evict_task_lock_if_unshared(task_id, &lock).await;
    }

    /// [`Self::settle_pause`] under the task lock the caller holds. The
    /// `paused` status and its `task_paused` event commit in one
    /// transaction (#314); any failure leaves the task `pausing`, logged at
    /// `error`, the safe side: the next pause, resume or restart sweep
    /// corrects it.
    pub(super) async fn settle_pause_locked(&self, task_id: &str) {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(task_id, %err, "could not read a pausing task to settle its pause; it stays pausing");
                return;
            }
        };
        if task.status != TASK_STATUS_PAUSING {
            return;
        }
        if self.has_running_work(task_id) {
            return;
        }
        let stage = match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => json!(state.current_stage),
            _ => Value::Null,
        };
        if let Err(err) = self.commit_paused(task_id, json!({ "stage": stage })).await {
            tracing::error!(
                task_id, %err,
                "could not mark a pausing task paused; it stays pausing"
            );
        }
    }

    /// Writes `paused` and its `task_paused` event in one transaction and
    /// wakes event waiters after the commit. Nothing is written when the task
    /// was no longer `pausing`. An error drops the transaction (rolling it
    /// back), so the task stays `pausing`.
    pub(super) async fn commit_paused(
        &self,
        task_id: &str,
        payload: Value,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if !tasks::mark_paused_in(&mut tx, task_id).await? {
            return Ok(());
        }
        db_events::append_for_task_on(&mut tx, task_id, EventType::TaskPaused, payload).await?;
        tx.commit().await?;
        self.events_notify.notify_waiters();
        Ok(())
    }

    // -----------------------------------------------------------------
    // resume
    // -----------------------------------------------------------------

    /// Resumes a `pausing` or `paused` task: writes `open` (with the poll
    /// window shift, in one transaction) and then starts what was held.
    pub async fn resume_task(
        self: &Arc<Self>,
        task_id: &str,
    ) -> Result<ResumeOutcome, ResumeTaskError> {
        let lock = self.lock_for_task(task_id).await;
        let result = {
            let _guard = lock.lock().await;
            self.resume_task_locked(task_id).await
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    pub(crate) async fn resume_task_locked(
        self: &Arc<Self>,
        task_id: &str,
    ) -> Result<ResumeOutcome, ResumeTaskError> {
        // Step 1 — reads only.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(ResumeTaskError::NoSuchTask)?;
        let refuse = |why: String| Err(ResumeTaskError::Refused(why));
        match task.status.as_str() {
            TASK_STATUS_PAUSING | TASK_STATUS_PAUSED => {}
            TASK_STATUS_STUCK => {
                return refuse(format!("task is stuck; use 'choco task retry {task_id}'"));
            }
            TASK_STATUS_OPEN => return refuse("task is not paused".to_string()),
            TASK_STATUS_CLOSED => return refuse("the task is closed".to_string()),
            TASK_STATUS_CANCELLED => return refuse("the task is cancelled".to_string()),
            other => return refuse(format!("task is '{other}', not paused")),
        }
        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(ResumeTaskError::NoWorkflowState)?;
        let definition = Arc::new(self.load_task_workflow(&task).await?);
        let stage = state.current_stage.clone();
        let stage_def = definition
            .stages
            .get(&stage)
            .ok_or_else(|| ResumeTaskError::UnknownStage(stage.clone()))?;
        let plan = self
            .plan_resume(&task, &definition, &stage, stage_def, &state)
            .await?;
        if let ResumePlan::Failed(why) = &plan {
            let reason = format!("stage '{stage}' cannot resume: {why}");
            return match self.mark_stuck(task_id, &reason, false).await {
                StuckMark::Marked => refuse(format!(
                    "{reason}; retry continues it (the task is now stuck)"
                )),
                StuckMark::WriteFailed => Err(ResumeTaskError::StuckWriteFailed(reason)),
                StuckMark::NotMarked => {
                    let now = tasks::get(&self.pool, task_id).await?;
                    let status = now.map_or("gone".to_string(), |t| t.status);
                    refuse(format!("{reason}; the task is '{status}' now"))
                }
            };
        }

        // Step 2 — one transaction: the poll window shift and `→ open`.
        let mut payload = state.payload.clone();
        let now = self.now();
        let since = match (task.paused_at, state.stage_entered_at) {
            (Some(paused), Some(entered)) => paused.max(entered),
            (Some(paused), None) => paused,
            (None, Some(entered)) => entered,
            (None, None) => now,
        };
        shift_poll_window(&mut payload, &stage, now - since);
        let mut tx = self.pool.begin().await?;
        if payload != state.payload {
            workflow_state::update_in(
                &mut tx,
                task_id,
                workflow_state::WorkflowStateUpdate {
                    current_stage: stage.clone(),
                    stage_kind: stage_def.kind.name().to_string(),
                    loop_counters: state.loop_counters.clone(),
                    payload: payload.clone(),
                    enters_stage: false,
                },
            )
            .await?
            .ok_or(ResumeTaskError::NoWorkflowState)?;
        }
        if !tasks::resume_in(&mut tx, task_id).await? {
            drop(tx);
            let status = tasks::get(&self.pool, task_id)
                .await?
                .map_or_else(|| "gone".to_string(), |t| t.status);
            return refuse(format!("task is '{status}', not paused"));
        }
        tx.commit().await?;

        // Step 3 — start what is held. A failure marks the task stuck.
        let mut started: Vec<String> = Vec::new();
        let mut resumed: Vec<String> = Vec::new();
        let started_result: Result<(), EngineError> = match plan {
            ResumePlan::Nothing => Ok(()),
            ResumePlan::Failed(_) => unreachable!("a failed plan returned above"),
            ResumePlan::Stage(resume) => {
                if resume.is_some() {
                    resumed.push(stage.clone());
                }
                started.push(stage.clone());
                let via = if resume.is_some() {
                    "resume_resume"
                } else {
                    "resume"
                };
                self.reenter_stage_locked(
                    task_id,
                    &definition,
                    &stage,
                    &payload,
                    via,
                    resume.as_ref(),
                )
                .await
            }
            ResumePlan::EnterGroup => {
                started.push(stage.clone());
                self.enter_stage(
                    task_id,
                    &definition,
                    &stage,
                    None,
                    Some("resume"),
                    &payload,
                    None,
                )
                .await
            }
            ResumePlan::Branches(restarts) => {
                for (name, resume) in &restarts {
                    started.push(name.clone());
                    if resume.is_some() {
                        resumed.push(name.clone());
                    }
                }
                let StageKind::Parallel { branches } = &stage_def.kind else {
                    unreachable!("a branch plan is only made for a parallel stage")
                };
                let entry_no = block_for(&payload, &stage)
                    .and_then(|block| block.get("entry"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                self.restart_branches_locked(
                    task_id,
                    &definition,
                    &stage,
                    branches,
                    entry_no,
                    &payload,
                    &restarts,
                )
                .await
            }
        };
        if let Err(err) = started_result {
            // `enter_stage` already appends its own `Error` event for a
            // template failure — see `mark_stuck`'s doc comment.
            let marked = self
                .mark_stuck(
                    task_id,
                    &format!("stage '{stage}': resume failed: {err}"),
                    matches!(err, EngineError::Template { .. }),
                )
                .await;
            return match marked {
                StuckMark::Marked => Err(ResumeTaskError::Enter(err)),
                StuckMark::WriteFailed => Err(ResumeTaskError::StartFailedNotStuck(format!(
                    "stage '{stage}': resume failed: {err}; marking the task stuck failed too, \
                     so it was left open with nothing started"
                ))),
                StuckMark::NotMarked => {
                    let now = tasks::get(&self.pool, task_id).await?;
                    let status = now.map_or("gone".to_string(), |t| t.status);
                    Err(ResumeTaskError::StartFailedNotStuck(format!(
                        "stage '{stage}': resume failed: {err}; the task is '{status}' now"
                    )))
                }
            };
        }
        self.wake_dispatcher();
        self.append_task_event(
            task_id,
            EventType::TaskResumed,
            json!({ "stage": stage, "started": started }),
        )
        .await;
        Ok(ResumeOutcome {
            stage,
            started,
            resumed,
        })
    }

    /// Decides what `resume` starts, from reads only. Anything still running
    /// or queued is skipped: a queue row is the dispatcher's, and claiming a
    /// stage or branch that has one would collide with it.
    async fn plan_resume(
        &self,
        task: &Task,
        definition: &WorkflowDefinition,
        stage: &str,
        stage_def: &StageDef,
        state: &WorkflowState,
    ) -> Result<ResumePlan, ResumeTaskError> {
        let task_id = task.id.as_str();
        match &stage_def.kind {
            StageKind::Terminal => Ok(ResumePlan::Nothing),
            StageKind::HumanGate { .. } => Ok(
                if stage_def.watch().is_some() && !self.has_detached_runner(task_id) {
                    ResumePlan::Stage(None)
                } else {
                    ResumePlan::Nothing
                },
            ),
            StageKind::Poll { .. } | StageKind::Shell { .. } => {
                Ok(if self.has_detached_runner(task_id) {
                    ResumePlan::Nothing
                } else {
                    ResumePlan::Stage(None)
                })
            }
            StageKind::AgentTurn { .. } => {
                if self.turn_gate.running_for_task(task_id) > 0
                    || turn_queue::exists(&self.pool, task_id, stage, None).await?
                {
                    return Ok(ResumePlan::Nothing);
                }
                let last = sessions::get_current_for_stage(&self.pool, task_id, stage).await?;
                let this_entry = last.filter(|s| {
                    state
                        .stage_entered_at
                        .is_none_or(|entered| s.started_at >= entered)
                });
                // A standing chat stage waits for a person.
                let standing = stage_def.on.is_empty();
                let Some(session) = this_entry else {
                    return Ok(ResumePlan::Stage(None));
                };
                if standing {
                    return Ok(ResumePlan::Nothing);
                }
                // Even when its watcher is still settling: the watcher would
                // park it stuck, and nothing else would move the task.
                if let Some(why) = run_failure(&session) {
                    return Ok(ResumePlan::Failed(why));
                }
                if !Self::pause_left_unsettled(&session) {
                    // A completed run whose result is not applied yet is
                    // moved on by its settling watcher; with none, nothing
                    // would ever move the task.
                    if self.has_running_work(task_id) {
                        return Ok(ResumePlan::Nothing);
                    }
                    return Ok(ResumePlan::Failed(format!(
                        "its last run, session {}, is {}/{}; {NOTHING_MOVES}",
                        session.id,
                        session.status,
                        reason_text(&session)
                    )));
                }
                let resume = self
                    .resumable_or_fresh(task, definition, stage_def, &session, false)
                    .await?;
                Ok(ResumePlan::Stage(resume))
            }
            StageKind::Parallel { branches } => {
                let entry_no = block_for(&state.payload, stage)
                    .and_then(|block| block.get("entry"))
                    .and_then(Value::as_i64);
                let Some(entry_no) = entry_no else {
                    return Ok(if self.has_running_work(task_id) {
                        ResumePlan::Nothing
                    } else {
                        ResumePlan::Failed(format!(
                            "the group has no recorded entry; {NOTHING_MOVES}"
                        ))
                    });
                };
                let mut any_started = false;
                for name in branches.keys() {
                    if db_events::branch_start_exists(&self.pool, task_id, name, entry_no).await? {
                        any_started = true;
                        break;
                    }
                }
                if !any_started {
                    return Ok(ResumePlan::EnterGroup);
                }
                let mut restarts = Vec::new();
                let mut work_remains = false;
                let mut found: Vec<String> = Vec::new();
                for (name, branch) in branches {
                    let slot = branch_slot_state(&state.payload, name);
                    if slot != Some("running") {
                        if slot != Some("done") {
                            found.push(format!("branch '{name}' is {}", slot.unwrap_or("unknown")));
                        }
                        continue;
                    }
                    match self.branch_progress(task_id, name, entry_no).await? {
                        BranchWork::Queued | BranchWork::Live(_) => work_remains = true,
                        BranchWork::Ended(session) => {
                            if let Some(why) = run_failure(&session) {
                                // Its watcher may still be about to record the
                                // end, and siblings may be running: leave it.
                                // A failed branch whose watcher is gone is the
                                // restart sweep's, for both pausing and paused
                                // tasks; if it was not recorded, resume fails
                                // the plan (restarting its siblings would
                                // leave a slot that never settles).
                                if self.turn_gate.busy_for_stage(task_id, name) {
                                    work_remains = true;
                                    continue;
                                }
                                return Ok(ResumePlan::Failed(format!(
                                    "branch '{name}' cannot resume: {why}"
                                )));
                            }
                            if Self::pause_left_unsettled(&session) {
                                let resume = self
                                    .resumable_or_fresh(
                                        task,
                                        definition,
                                        &branch.def,
                                        &session,
                                        true,
                                    )
                                    .await?;
                                restarts.push((name.clone(), resume));
                            } else if self.turn_gate.busy_for_stage(task_id, name) {
                                // A completed run its watcher is still applying.
                                work_remains = true;
                            } else {
                                // Completed but never applied, and no watcher:
                                // restarting its siblings would leave a slot
                                // that never settles.
                                return Ok(ResumePlan::Failed(format!(
                                    "branch '{name}' completed (run {}, {}/{}) but its result was never applied; {NOTHING_MOVES}",
                                    session.id,
                                    session.status,
                                    reason_text(&session)
                                )));
                            }
                        }
                        BranchWork::NeverStarted => restarts.push((name.clone(), None)),
                    }
                }
                if !restarts.is_empty() {
                    return Ok(ResumePlan::Branches(restarts));
                }
                if work_remains || self.has_running_work(task_id) {
                    return Ok(ResumePlan::Nothing);
                }
                if found.is_empty() {
                    found.push("no branch has anything left to restart".to_string());
                }
                Ok(ResumePlan::Failed(format!(
                    "{}; {NOTHING_MOVES}",
                    found.join("; ")
                )))
            }
        }
    }

    /// Whether anything of the task's is running or settling, or a detached
    /// runner is registered.
    fn has_running_work(&self, task_id: &str) -> bool {
        self.turn_gate.busy_for_task(task_id) > 0 || self.has_detached_runner(task_id)
    }

    /// Whether a pause (or the daemon stopping under the task) ended this
    /// session without its watcher settling anything: those are the only
    /// ends `resume` restarts. A live or completing turn belongs to its
    /// watcher, and a failure is `run_failure`'s.
    fn pause_left_unsettled(session: &Session) -> bool {
        session.status == SessionStatus::Exited
            && matches!(
                session.end_reason,
                Some(SessionEndReason::Paused | SessionEndReason::DaemonStopped)
            )
    }

    /// The session to resume, or `None` to start fresh. A database error is
    /// an error; "cannot be resumed" is not.
    async fn resumable_or_fresh(
        &self,
        task: &Task,
        definition: &WorkflowDefinition,
        stage_def: &StageDef,
        session: &Session,
        is_branch: bool,
    ) -> Result<Option<ResumeSession>, ResumeTaskError> {
        match self
            .resumable_session(task, definition, stage_def, Some(session), is_branch)
            .await
        {
            Ok(Ok(resume)) => Ok(Some(resume)),
            Ok(Err(why)) => {
                tracing::info!(task_id = %task.id, why, "resuming with a fresh session");
                Ok(None)
            }
            Err(RetryTaskError::Db(err)) => Err(ResumeTaskError::Db(err)),
            Err(err) => {
                tracing::warn!(task_id = %task.id, %err, "could not tell whether a session can be resumed; starting fresh");
                Ok(None)
            }
        }
    }

    /// Re-enters `stage` of a task whose status is `open`, under the task
    /// lock the caller holds: works out a `prompt_file`-less agent turn's
    /// input and calls `enter_stage` (not `advance`, which would take the
    /// lock again). Shared by `retry` and `resume`; `entered_via` is what the
    /// timeline shows after the `--[` and `]-->`.
    pub(super) async fn reenter_stage_locked(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage: &str,
        payload: &Value,
        entered_via: &str,
        resume: Option<&ResumeSession>,
    ) -> Result<(), EngineError> {
        let stage_def = definition
            .stages
            .get(stage)
            .ok_or_else(|| EngineError::UnknownStage(stage.to_string()))?;
        let input = match &stage_def.kind {
            StageKind::AgentTurn {
                prompt_file: None, ..
            } => payload
                .get("task")
                .and_then(|task| task.get("input"))
                .and_then(Value::as_str),
            _ => None,
        };
        self.enter_stage(
            task_id,
            definition,
            stage,
            input,
            Some(entered_via),
            payload,
            resume,
        )
        .await
    }

    /// Restarts the given branches of the group `group`, under the task lock
    /// the caller holds: records their starts (`via` resume), starts each,
    /// and records the ones that failed to start.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn restart_branches_locked(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        group: &str,
        branches: &IndexMap<String, Branch>,
        entry_no: i64,
        payload: &Value,
        restarts: &[(String, Option<ResumeSession>)],
    ) -> Result<(), EngineError> {
        let input = payload
            .get("task")
            .and_then(|task| task.get("input"))
            .and_then(Value::as_str);
        let recorded = async {
            let mut tx = self.pool.begin().await?;
            for (name, resume) in restarts {
                let Some(branch) = branches.get(name) else {
                    continue;
                };
                let via = if resume.is_some() {
                    "resume_resume"
                } else {
                    "resume"
                };
                db_events::append_branch_started_on(
                    &mut tx,
                    task_id,
                    group,
                    name,
                    branch.def.kind.name(),
                    entry_no,
                    Some(via),
                )
                .await?;
            }
            tx.commit().await
        }
        .await;
        let mut failures: Vec<(String, String)> = Vec::new();
        if let Err(err) = recorded {
            tracing::error!(task_id, group, %err, "could not record the resumed branch starts");
            for (name, _) in restarts {
                failures.push((
                    name.clone(),
                    format!("could not record the branch start: {err}"),
                ));
            }
        } else {
            for (name, resume) in restarts {
                let Some(branch) = branches.get(name) else {
                    failures.push((
                        name.clone(),
                        "the branch is no longer in the workflow".into(),
                    ));
                    continue;
                };
                if !matches!(branch.def.kind, StageKind::AgentTurn { .. }) {
                    failures.push((
                        name.clone(),
                        format!(
                            "branch '{name}' is not an agent_turn, which is all a branch can be"
                        ),
                    ));
                    continue;
                }
                let branch_entry = StageEntry {
                    task_id,
                    definition,
                    stage_name: name,
                    stage_def: &branch.def,
                    payload,
                    input,
                    resume: resume.as_ref(),
                    branch: Some(BranchEntry {
                        group,
                        entry: entry_no,
                        results: &branch.results,
                    }),
                };
                if let Err(err) = self.enter_agent_turn(&branch_entry).await {
                    tracing::error!(task_id, group, branch = %name, %err, "branch failed to start");
                    failures.push((name.clone(), err.to_string()));
                }
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        self.record_start_failures(task_id, group, branches, entry_no, &failures)
            .await
    }

    // -----------------------------------------------------------------
    // a requested end: the watcher's side of pause (#295)
    // -----------------------------------------------------------------

    /// Re-enters a stage or branch whose session was killed by a pause that
    /// has since been resumed (the `resume` found the session still draining
    /// and left it to its watcher). Does nothing unless the task is `open`
    /// and the session is still the stage's current run with nothing running
    /// or queued: `resume` may have restarted it already.
    pub(super) async fn reenter_after_pause(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        session_id: &str,
        branch: Option<&super::parallel::BranchWatch>,
    ) {
        let lock = self.lock_for_task(task_id).await;
        {
            let _guard = lock.lock().await;
            if let Err(why) = self
                .reenter_after_pause_locked(task_id, definition, stage_name, session_id, branch)
                .await
            {
                self.mark_stuck(
                    task_id,
                    &format!("stage '{stage_name}': resume failed: {why}"),
                    false,
                )
                .await;
            }
        }
        self.evict_task_lock_if_unshared(task_id, &lock).await;
    }

    async fn reenter_after_pause_locked(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        session_id: &str,
        branch: Option<&super::parallel::BranchWatch>,
    ) -> Result<(), String> {
        let err = |e: sqlx::Error| e.to_string();
        let Some(task) = tasks::get(&self.pool, task_id).await.map_err(err)? else {
            return Ok(());
        };
        if task.status != TASK_STATUS_OPEN {
            return Ok(());
        }
        let Some(state) = workflow_state::get(&self.pool, task_id)
            .await
            .map_err(err)?
        else {
            return Ok(());
        };
        let Some(session) = sessions::get(&self.pool, session_id).await.map_err(err)? else {
            return Ok(());
        };
        let current = sessions::get_current_for_stage(&self.pool, task_id, stage_name)
            .await
            .map_err(err)?;
        if current.as_ref().map(|s| s.id.as_str()) != Some(session_id)
            || turn_queue::exists(&self.pool, task_id, stage_name, None)
                .await
                .map_err(err)?
        {
            return Ok(());
        }
        match branch {
            None => {
                if state.current_stage != stage_name || self.turn_gate.running_for_task(task_id) > 0
                {
                    return Ok(());
                }
                let Some(stage_def) = definition.stages.get(stage_name) else {
                    return Ok(());
                };
                let resume = self
                    .resumable_or_fresh(&task, definition, stage_def, &session, false)
                    .await
                    .map_err(|e| e.to_string())?;
                let via = if resume.is_some() {
                    "resume_resume"
                } else {
                    "resume"
                };
                self.reenter_stage_locked(
                    task_id,
                    definition,
                    stage_name,
                    &state.payload,
                    via,
                    resume.as_ref(),
                )
                .await
                .map_err(|e| e.to_string())
            }
            Some(watch) => {
                let group = watch.group.as_str();
                let still_current = state.current_stage == group
                    && block_for(&state.payload, group)
                        .and_then(|b| b.get("entry"))
                        .and_then(Value::as_i64)
                        == Some(watch.entry)
                    && branch_slot_state(&state.payload, stage_name) == Some("running");
                if !still_current {
                    return Ok(());
                }
                let Some(StageKind::Parallel { branches }) =
                    definition.stages.get(group).map(|d| &d.kind)
                else {
                    return Ok(());
                };
                let Some(branch_def) = branches.get(stage_name) else {
                    return Ok(());
                };
                let resume = self
                    .resumable_or_fresh(&task, definition, &branch_def.def, &session, true)
                    .await
                    .map_err(|e| e.to_string())?;
                self.restart_branches_locked(
                    task_id,
                    definition,
                    group,
                    branches,
                    watch.entry,
                    &state.payload,
                    &[(stage_name.to_string(), resume)],
                )
                .await
                .map_err(|e| e.to_string())
            }
        }
    }

    /// Which verdict a requested end (Paused, or the daemon stopping) of a
    /// pausing or paused task's turn gets: only a read-only role that changed
    /// the worktree makes the *task* stuck. Nothing is parked and no branch
    /// is failed.
    pub(super) async fn check_pause_end(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        session_id: &str,
        branch: Option<&super::parallel::BranchWatch>,
    ) {
        match self
            .read_only_verdict(task_id, definition, stage_name, session_id, false)
            .await
        {
            ReadOnlyVerdict::Clean => {}
            ReadOnlyVerdict::Violation(found) | ReadOnlyVerdict::Unverified(found) => {
                let beside = branch
                    .map(|watch| super::parallel::ran_beside(definition, &watch.group, stage_name))
                    .unwrap_or_default();
                self.mark_stuck(task_id, &format!("{found}{beside}"), false)
                    .await;
            }
        }
    }

    /// The count of detached runners of `kind` registered for `task_id`.
    pub(super) fn detached_runner_count(&self, task_id: &str, kind: DetachedKind) -> usize {
        self.detached_runners
            .lock()
            .expect("detached_runners mutex poisoned")
            .get(task_id)
            .map_or(0, |task| task.values().filter(|(k, _)| *k == kind).count())
    }
}
