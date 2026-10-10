use super::turn::{ReadOnlyVerdict, unverified_note};
use super::watch::{PollWindow, poll_window_for, remaining_budget, set_poll_window};
use super::*;

/// Whether a stage's work survives a daemon restart. The one place this is
/// decided: the startup park sweep and `GET /server`'s `in_flight` both use
/// it, so they cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartEffect {
    /// Waiting state lives in the database (or the stage is standing), so a
    /// restart loses nothing.
    Survives,
    /// A single-shot agent turn: its process dies with the daemon.
    StrandsAgentTurn,
    /// A shell command: its process dies with the daemon.
    StrandsShell,
}

/// Exhaustive on purpose: a new stage kind must be classified here to
/// compile.
pub fn restart_effect(def: &StageDef) -> RestartEffect {
    match &def.kind {
        // Standing (chat) sessions resume by message instead.
        StageKind::AgentTurn { .. } if def.on.is_empty() => RestartEffect::Survives,
        StageKind::AgentTurn { .. } => RestartEffect::StrandsAgentTurn,
        StageKind::Shell { .. } => RestartEffect::StrandsShell,
        StageKind::Poll { .. } | StageKind::HumanGate { .. } | StageKind::Terminal => {
            RestartEffect::Survives
        }
    }
}

pub(super) fn agent_reason(stage: &str) -> String {
    format!(
        "stage '{stage}' was running an agent turn when the daemon stopped; 'choco task retry' \
         continues it, resuming the agent's session when it can"
    )
}

fn shell_reason(stage: &str) -> String {
    format!(
        "stage '{stage}' was running a shell command when the daemon stopped; 'choco task retry' \
         runs it again from the start"
    )
}

/// What [`WorkflowEngine::park_interrupted_turns`] did, per task.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParkReport {
    /// Single-shot agent turns parked as stuck. Counts only tasks whose status actually changed to `stuck`; a task already
    /// no longer open, or whose stuck write failed (logged by `mark_stuck`), is
    /// not counted.
    pub agent_turns: usize,
    /// Shell stages parked as stuck. Counts only tasks whose status actually changed to `stuck`; a task already
    /// no longer open, or whose stuck write failed (logged by `mark_stuck`), is
    /// not counted.
    pub shells: usize,
    /// Tasks parked because the sweep could not classify them. Counts only tasks whose status actually changed to `stuck`; a task already
    /// no longer open, or whose stuck write failed (logged by `mark_stuck`), is
    /// not counted.
    pub stuck_other: usize,
}

/// What [`WorkflowEngine::resume_interrupted_polls`] did, per task.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PollSweepReport {
    /// Re-entered with the deadline they already had.
    pub resumed: usize,
    /// Skipped: a live runner already owns the task.
    pub already_running: usize,
    /// Could not be resumed, and were marked stuck. Counts only tasks whose status actually changed to `stuck`; a task already
    /// no longer open, or whose stuck write failed (logged by `mark_stuck`), is
    /// not counted.
    pub stuck: usize,
    /// Tasks whose stored `stage_kind` was missing or stale and could not be
    /// corrected. Each was logged; the sweep carried on with the resume.
    pub stage_kind_unrecorded: usize,
}

impl WorkflowEngine {
    /// Startup sweep (#52): re-enters every `open` task sitting in a stage
    /// with a watcher — a `poll`, or a `human_gate` with a `watch:` (#175) —
    /// whose runner died with the previous process, with the deadline it
    /// already had. It also records the `stage_kind` of every open task it
    /// loads whose stored kind is missing or differs from the workflow's.
    /// Per task it holds the per-task lock, so the ownership check
    /// (`has_detached_runner`) and the resume's own spawn can't be
    /// interleaved by another spawner.
    ///
    /// Every per-task failure ends in `mark_stuck`; one task's failure
    /// never stops the sweep. Only a failure to list the candidates is
    /// returned.
    pub async fn resume_interrupted_polls(
        self: &Arc<Self>,
    ) -> Result<PollSweepReport, sqlx::Error> {
        let candidates = tasks::list(&self.pool, None, Some(TASK_STATUS_OPEN)).await?;
        let mut report = PollSweepReport::default();
        for task in candidates {
            let lock = self.lock_for_task(&task.id).await;
            {
                let _guard = lock.lock().await;
                self.resume_interrupted_poll_locked(&task.id, &mut report)
                    .await;
            }
            self.evict_task_lock_if_unshared(&task.id, &lock).await;
        }
        Ok(report)
    }

    /// Startup sweep (#84): any `open` task sitting in an `agent_turn` or
    /// `shell` stage lost its process when the previous daemon stopped, so
    /// it is parked `stuck` (retry continues or re-runs it) rather than
    /// left `open` forever. Must run before [`Self::resume_interrupted_polls`]:
    /// a resumed poll can advance into an `agent_turn`, and that live turn
    /// must not be parked.
    ///
    /// Shaped like the poll sweep: per task under its lock, every per-task
    /// failure ends in `mark_stuck` naming it, and only a failure to list
    /// the candidates is returned.
    pub async fn park_interrupted_turns(self: &Arc<Self>) -> Result<ParkReport, sqlx::Error> {
        let candidates = tasks::list(&self.pool, None, Some(TASK_STATUS_OPEN)).await?;
        let mut report = ParkReport::default();
        for task in candidates {
            let lock = self.lock_for_task(&task.id).await;
            {
                let _guard = lock.lock().await;
                self.park_interrupted_turn_locked(&task.id, &mut report)
                    .await;
            }
            self.evict_task_lock_if_unshared(&task.id, &lock).await;
        }
        Ok(report)
    }

    async fn park_mark_stuck(&self, task_id: &str, reason: &str, report: &mut ParkReport) {
        if self.mark_stuck(task_id, reason, false).await == StuckMark::Marked {
            report.stuck_other += 1;
        }
    }

    /// The body of [`Self::park_interrupted_turns`] for one task; the caller
    /// holds its per-task lock.
    async fn park_interrupted_turn_locked(&self, task_id: &str, report: &mut ParkReport) {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                let reason = format!("restart sweep could not read the task: {err}");
                self.park_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };
        if task.status != TASK_STATUS_OPEN {
            return;
        }
        let state = match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => state,
            Ok(None) => return,
            Err(err) => {
                let reason =
                    format!("restart sweep could not read the task's workflow state: {err}");
                self.park_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };
        let stage = state.current_stage.clone();
        let definition = match self.load_task_workflow(&task).await {
            Ok(definition) => definition,
            Err(err) => {
                let reason = format!(
                    "the daemon restarted and could not load this task's workflow to check \
                     whether stage '{stage}' was interrupted: {err}; fix the workflow file, \
                     then retry"
                );
                self.park_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };
        let Some(stage_def) = definition.stages.get(&stage) else {
            let reason = format!("stage '{stage}' no longer exists in the task's workflow");
            self.park_mark_stuck(task_id, &reason, report).await;
            return;
        };
        match restart_effect(stage_def) {
            RestartEffect::Survives => {}
            RestartEffect::StrandsShell => {
                if self.mark_stuck(task_id, &shell_reason(&stage), false).await == StuckMark::Marked
                {
                    report.shells += 1;
                }
            }
            RestartEffect::StrandsAgentTurn => {
                let mut reason = agent_reason(&stage);
                match sessions::get_current_for_stage(&self.pool, task_id, &stage).await {
                    Ok(Some(session)) => {
                        if let Err(err) =
                            sessions::mark_daemon_stopped(&self.pool, &session.id).await
                        {
                            reason.push_str(&format!(
                                "; the interrupted session could not be recorded ({err}), so \
                                 retry will start the stage fresh"
                            ));
                        }
                        // A read-only role may have changed the worktree
                        // before the daemon stopped (#172).
                        match self
                            .read_only_verdict(task_id, &definition, &stage, &session.id, false)
                            .await
                        {
                            ReadOnlyVerdict::Clean => {}
                            ReadOnlyVerdict::Violation(found)
                            | ReadOnlyVerdict::Unverified(found) => {
                                reason.push_str(&format!("; {found}"));
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        reason.push_str(&format!(
                            "; the interrupted session could not be recorded ({err}), so \
                             retry will start the stage fresh"
                        ));
                        // Without the session there is no baseline to
                        // compare, and a fresh retry would take whatever is
                        // in the worktree as its baseline (#172).
                        if let Some(note) = unverified_note(&definition, &stage, &err) {
                            reason.push_str(&format!("; {note}"));
                        }
                    }
                }
                if self.mark_stuck(task_id, &reason, false).await == StuckMark::Marked {
                    report.agent_turns += 1;
                }
            }
        }
    }

    /// The `open` tasks a restart would strand right now (`GET /server`).
    /// A workflow that cannot be loaded, or a stage it no longer has, is
    /// listed with kind `"unknown"` rather than hidden.
    pub async fn in_flight(&self) -> Result<Vec<InFlight>, sqlx::Error> {
        let mut out = Vec::new();
        for task in tasks::list(&self.pool, None, Some(TASK_STATUS_OPEN)).await? {
            let Some(state) = workflow_state::get(&self.pool, &task.id).await? else {
                continue;
            };
            let stage = state.current_stage;
            let kind = match self.load_task_workflow(&task).await {
                Ok(definition) => match definition.stages.get(&stage).map(restart_effect) {
                    Some(RestartEffect::Survives) => continue,
                    Some(RestartEffect::StrandsAgentTurn) => "agent_turn",
                    Some(RestartEffect::StrandsShell) => "shell",
                    None => "unknown",
                },
                Err(err) => {
                    tracing::warn!(
                        task_id = %task.id,
                        error = %err,
                        "could not load the task's workflow; listing it as unknown in-flight"
                    );
                    "unknown"
                }
            };
            out.push(InFlight {
                task_id: task.id,
                title: task.title,
                stage,
                kind: kind.to_string(),
            });
        }
        Ok(out)
    }

    async fn sweep_mark_stuck(&self, task_id: &str, reason: &str, report: &mut PollSweepReport) {
        if self.mark_stuck(task_id, reason, false).await == StuckMark::Marked {
            report.stuck += 1;
        }
    }

    /// The body of [`Self::resume_interrupted_polls`] for one task; the
    /// caller holds its per-task lock.
    async fn resume_interrupted_poll_locked(
        self: &Arc<Self>,
        task_id: &str,
        report: &mut PollSweepReport,
    ) {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                let reason = format!("poll sweep could not read the task: {err}");
                self.sweep_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };
        if task.status != TASK_STATUS_OPEN {
            return;
        }
        let state = match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => state,
            Ok(None) => return,
            Err(err) => {
                let reason = format!("poll sweep could not read the task's workflow state: {err}");
                self.sweep_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };
        let stage = state.current_stage.clone();
        let window = poll_window_for(&state.payload, &stage);
        let was_polling = !matches!(window, Ok(None));

        let definition = match self.load_task_workflow(&task).await {
            Ok(definition) => Arc::new(definition),
            Err(err) => {
                if was_polling {
                    let reason = format!(
                        "stage '{stage}' was polling when the daemon stopped, but its workflow \
                         could not be loaded: {err}"
                    );
                    self.sweep_mark_stuck(task_id, &reason, report).await;
                } else {
                    tracing::warn!(
                        task_id, %err,
                        "poll sweep skipped a task whose workflow could not be loaded"
                    );
                }
                return;
            }
        };
        let Some(stage_def) = definition.stages.get(&stage) else {
            if was_polling {
                let reason = format!(
                    "stage '{stage}' was polling when the daemon stopped, but the workflow no \
                     longer defines it; retry to run it as defined"
                );
                self.sweep_mark_stuck(task_id, &reason, report).await;
            }
            return;
        };

        // Fills in a row from before `stage_kind` existed, or one a workflow
        // edit made stale. Single-column write: `updated_at` is untouched, as
        // the window derivation below reads it. A failure is logged and
        // counted, never silent, and does not stop the resume.
        let kind = stage_def.kind.name();
        if state.stage_kind.as_deref() != Some(kind)
            && let Err(err) = workflow_state::set_stage_kind(&self.pool, task_id, kind).await
        {
            tracing::error!(task_id, stage, %err, "could not record the task's stage kind");
            report.stage_kind_unrecorded += 1;
        }

        if stage_def.watch().is_none() {
            if was_polling {
                let reason = format!(
                    "stage '{stage}' was polling when the daemon stopped, but the workflow no \
                     longer gives it a watcher; retry to run it as defined"
                );
                self.sweep_mark_stuck(task_id, &reason, report).await;
            }
            return;
        }

        if self.has_detached_runner(task_id) {
            report.already_running += 1;
            return;
        }

        let payload = match window {
            Ok(Some(_)) => state.payload.clone(),
            Ok(None) => {
                // A row written before the window existed: derive one in
                // memory from when the row last changed. Not persisted.
                let mut payload = state.payload.clone();
                if let Err(err) =
                    set_poll_window(&mut payload, &definition, &stage, state.updated_at)
                {
                    let reason = format!(
                        "stage '{stage}': poll could not be resumed after a daemon restart: {err}"
                    );
                    self.sweep_mark_stuck(task_id, &reason, report).await;
                    return;
                }
                payload
            }
            Err(err) => {
                let reason = format!(
                    "stage '{stage}': poll could not be resumed after a daemon restart: {err}"
                );
                self.sweep_mark_stuck(task_id, &reason, report).await;
                return;
            }
        };

        // A restart is not a transition: `arrival` is left alone, as retry
        // leaves it.
        match self
            .enter_stage(
                task_id,
                &definition,
                &stage,
                None,
                Some("restart"),
                &payload,
                None,
            )
            .await
        {
            Ok(()) => {
                report.resumed += 1;
                let remaining = match poll_window_for(&payload, &stage) {
                    Ok(Some(PollWindow {
                        deadline: Some(at), ..
                    })) => format!("{:?}", remaining_budget(at, self.now())),
                    _ => "unbounded".to_string(),
                };
                tracing::info!(
                    task_id,
                    stage,
                    remaining,
                    "resumed an interrupted poll stage"
                );
            }
            Err(err) => {
                let reason = format!(
                    "stage '{stage}': poll could not be resumed after a daemon restart: {err}"
                );
                let mark = self
                    .mark_stuck(
                        task_id,
                        &reason,
                        matches!(err, EngineError::Template { .. }),
                    )
                    .await;
                if mark == StuckMark::Marked {
                    report.stuck += 1;
                }
            }
        }
    }
}
