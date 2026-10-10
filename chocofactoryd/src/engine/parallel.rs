//! Running a `kind: parallel` group (#257, PG1-4): entering it, starting every
//! branch, finishing each branch as its turn ends, and settling the group
//! once no branch is running.
//!
//! The branch state is engine-owned payload (`payload.parallel`), written in
//! the same UPDATE that records each fact, always under the per-task lock.
//! Every read that feeds a write here is made under that lock and outside any
//! transaction; the only transactions are `settle_with_failures` (whose first
//! statement is its UPDATE) and the `branch_started` batch (INSERTs only), so
//! deferred transactions are safe.

use super::*;
use crate::db::events::BranchEnd;
use crate::workflow_def::Branch;

/// What `enter_agent_turn` needs to know when the stage it starts is a
/// parallel group's branch rather than a top-level stage.
#[derive(Clone, Copy)]
pub(super) struct BranchEntry<'a> {
    pub(super) group: &'a str,
    pub(super) entry: i64,
    pub(super) results: &'a [String],
}

/// A branch's turn watcher's view of where it belongs: the group and the
/// group entry it was started under.
#[derive(Debug, Clone)]
pub(super) struct BranchWatch {
    pub(super) group: String,
    pub(super) entry: i64,
}

/// How a branch's turn ended, as handed to [`WorkflowEngine::finish_branch`].
pub(super) enum BranchOutcome {
    Done {
        result: String,
        capture: Option<Value>,
        /// Why `result` is a fallback, when it is one.
        note: Option<String>,
    },
    Failed {
        reason: String,
    },
}

/// What `finish_branch` did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BranchApplied {
    /// The branch's end was written. `done` is false when it was recorded as
    /// failed.
    Recorded { done: bool },
    /// Nothing was written: the end no longer applies. Carries why.
    Dropped(String),
    /// Writing failed and the task was parked. Carries the error.
    Parked(String),
}

fn stamp(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Writes `payload.parallel` (and bumps `payload.parallel_entries`) when
/// `stage` is a parallel group, with every branch `running`; removes the
/// `parallel` key otherwise. Called beside `set_poll_window` wherever a task
/// is moved into a stage, in the same write.
pub(super) fn set_parallel_block(
    payload: &mut Value,
    definition: &WorkflowDefinition,
    stage: &str,
    now: DateTime<Utc>,
) {
    if !payload.is_object() {
        *payload = json!({});
    }
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let Some(StageKind::Parallel { branches }) = definition.stages.get(stage).map(|d| &d.kind)
    else {
        object.remove("parallel");
        return;
    };
    let entry = object
        .get("parallel_entries")
        .and_then(|entries| entries.get(stage))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    let entries = object
        .entry("parallel_entries")
        .or_insert_with(|| json!({}));
    if !entries.is_object() {
        *entries = json!({});
    }
    if let Some(entries) = entries.as_object_mut() {
        entries.insert(stage.to_string(), json!(entry));
    }
    let started_at = stamp(now);
    let branch_states: serde_json::Map<String, Value> = branches
        .keys()
        .map(|name| {
            (
                name.clone(),
                json!({ "state": "running", "started_at": started_at }),
            )
        })
        .collect();
    object.insert(
        "parallel".to_string(),
        json!({ "stage": stage, "entry": entry, "branches": branch_states }),
    );
}

/// The `parallel` block when it belongs to `group`.
fn block_for<'a>(payload: &'a Value, group: &str) -> Option<&'a Value> {
    payload
        .get("parallel")
        .filter(|block| block.get("stage").and_then(Value::as_str) == Some(group))
}

fn branch_state<'a>(payload: &'a Value, branch: &str) -> Option<&'a str> {
    payload
        .get("parallel")?
        .get("branches")?
        .get(branch)?
        .get("state")?
        .as_str()
}

/// Replaces a branch's slot with `fields`, keeping its `started_at` and
/// adding `ended_at`.
fn end_branch(payload: &mut Value, branch: &str, mut fields: Value, now: &str) {
    let slot = payload
        .get_mut("parallel")
        .and_then(|p| p.get_mut("branches"))
        .and_then(|b| b.get_mut(branch));
    if let Some(slot) = slot {
        fields["started_at"] = slot.get("started_at").cloned().unwrap_or(Value::Null);
        fields["ended_at"] = json!(now);
        *slot = fields;
    }
}

fn fail_branch(payload: &mut Value, branch: &str, reason: &str, resumable: bool, now: &str) {
    end_branch(
        payload,
        branch,
        json!({ "state": "failed", "reason": reason, "resumable": resumable }),
        now,
    );
}

/// The stuck reason for a group that settled with failures, naming each
/// failed branch in declaration order.
fn settle_reason(group: &str, branches: &IndexMap<String, Branch>, payload: &Value) -> String {
    let failed: Vec<String> = branches
        .keys()
        .filter_map(|name| {
            let slot = payload
                .get("parallel")?
                .get("branches")?
                .get(name.as_str())?;
            (slot.get("state").and_then(Value::as_str) == Some("failed")).then(|| {
                let reason = slot.get("reason").and_then(Value::as_str).unwrap_or("");
                format!("'{name}': {reason}")
            })
        })
        .collect();
    format!(
        "parallel stage '{group}': {} of {} branch(es) failed: {}",
        failed.len(),
        branches.len(),
        failed.join("; ")
    )
}

/// `"; ran beside: a, b"` naming every other branch declared in `group`.
pub(super) fn ran_beside(definition: &WorkflowDefinition, group: &str, branch: &str) -> String {
    let Some(StageKind::Parallel { branches }) = definition.stages.get(group).map(|d| &d.kind)
    else {
        return String::new();
    };
    let others: Vec<&str> = branches
        .keys()
        .map(String::as_str)
        .filter(|name| *name != branch)
        .collect();
    if others.is_empty() {
        String::new()
    } else {
        format!("; ran beside: {}", others.join(", "))
    }
}

/// The role of the agent stage or branch `name`.
pub(super) fn agent_role_of<'a>(definition: &'a WorkflowDefinition, name: &str) -> Option<&'a str> {
    let kind = match definition.stages.get(name) {
        Some(stage) => &stage.kind,
        None => &definition.branch(name)?.def.kind,
    };
    match kind {
        StageKind::AgentTurn { role, .. } => Some(role.as_str()),
        _ => None,
    }
}

impl WorkflowEngine {
    /// Best-effort `branch_finished` timeline entry, after the commit that
    /// recorded the end.
    async fn record_branch_finished(
        &self,
        task_id: &str,
        group: &str,
        branch: &str,
        entry: i64,
        end: BranchEnd<'_>,
    ) {
        match events::append_branch_finished(&self.pool, task_id, group, branch, entry, end).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, group, branch, %err,
                "failed to record a branch_finished event"
            ),
        }
    }

    async fn record_branch_end(
        &self,
        task_id: &str,
        group: &str,
        branch: &str,
        entry: i64,
        ended: &Result<String, String>,
    ) {
        let end = match ended {
            Ok(result) => BranchEnd::Done { result },
            Err(reason) => BranchEnd::Failed { reason },
        };
        self.record_branch_finished(task_id, group, branch, entry, end)
            .await;
    }

    /// Commits a group's failure settle (payload + `stuck` in one
    /// transaction). Returns the stuck reason when the task was marked stuck;
    /// the caller appends the `Error` event (see
    /// [`Self::append_settle_error`]) after the branches' `branch_finished`.
    async fn settle_group_with_failures(
        &self,
        task_id: &str,
        group: &str,
        branches: &IndexMap<String, Branch>,
        update: workflow_state::WorkflowStateUpdate,
    ) -> Result<Option<String>, EngineError> {
        let reason = settle_reason(group, branches, &update.payload);
        let settled = workflow_state::settle_with_failures(&self.pool, task_id, update, &reason)
            .await?
            .ok_or(EngineError::NoWorkflowState)?;
        if !settled.marked_stuck {
            tracing::info!(
                task_id,
                group,
                reason,
                "parallel group settled with failures but the task was no longer open"
            );
            return Ok(None);
        }
        Ok(Some(reason))
    }

    /// Appends the settle's `Error` event, best-effort, as `mark_stuck` does.
    async fn append_settle_error(&self, task_id: &str, group: &str, reason: String) {
        tracing::error!(task_id, reason, "task stuck: {reason}");
        match events::append_for_task(
            &self.pool,
            task_id,
            EventType::Error,
            json!({ "stage": group, "message": reason, "stuck": true }),
        )
        .await
        {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(task_id, %err, "failed to record a stuck-task event"),
        }
    }

    /// Enters a parallel group under the task lock the caller holds: records
    /// every branch start, then starts each branch. See the module docs.
    pub(super) async fn enter_group(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name: group,
            stage_def,
            payload,
            input,
            ..
        } = *entry;
        let StageKind::Parallel { branches } = &stage_def.kind else {
            unreachable!("enter_group is only called for Parallel stages")
        };
        let Some(entry_no) = block_for(payload, group)
            .and_then(|block| block.get("entry"))
            .and_then(Value::as_i64)
        else {
            return Err(EngineError::GroupStateMissing {
                stage: group.to_string(),
            });
        };

        // Every start is recorded before any branch session row exists: a
        // session's lap is counted from these events when its row is
        // inserted. One transaction, INSERTs only, so a deferred BEGIN is
        // safe; all or none.
        if let Err(err) = self
            .record_branch_starts(task_id, group, branches, entry_no)
            .await
        {
            tracing::error!(task_id, group, %err, "could not record the branch starts");
            let failures: Vec<(String, String)> = branches
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        format!("could not record the branch start: {err}"),
                    )
                })
                .collect();
            return self
                .record_start_failures(task_id, group, branches, entry_no, &failures)
                .await;
        }

        let mut failures: Vec<(String, String)> = Vec::new();
        for (name, branch) in branches {
            if !matches!(branch.def.kind, StageKind::AgentTurn { .. }) {
                failures.push((
                    name.clone(),
                    format!("branch '{name}' is not an agent_turn, which is all a branch can be"),
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
                resume: None,
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
        if failures.is_empty() {
            return Ok(());
        }
        self.record_start_failures(task_id, group, branches, entry_no, &failures)
            .await
    }

    async fn record_branch_starts(
        &self,
        task_id: &str,
        group: &str,
        branches: &IndexMap<String, Branch>,
        entry_no: i64,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (name, branch) in branches {
            events::append_branch_started_on(
                &mut tx,
                task_id,
                group,
                name,
                branch.def.kind.name(),
                entry_no,
                None,
            )
            .await?;
        }
        tx.commit().await
    }

    /// Marks every branch in `failures` failed in one UPDATE (re-reading the
    /// state under the lock the caller holds); that UPDATE is the settle when
    /// no branch is left running.
    async fn record_start_failures(
        &self,
        task_id: &str,
        group: &str,
        branches: &IndexMap<String, Branch>,
        entry_no: i64,
        failures: &[(String, String)],
    ) -> Result<(), EngineError> {
        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoWorkflowState)?;
        let mut payload = state.payload;
        let now = stamp(self.now());
        for (name, reason) in failures {
            fail_branch(&mut payload, name, reason, false, &now);
        }
        let any_running = branches
            .keys()
            .any(|name| branch_state(&payload, name) == Some("running"));
        let update = workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage,
            stage_kind: "parallel".to_string(),
            loop_counters: state.loop_counters,
            payload,
            enters_stage: false,
        };
        let stuck_reason = if any_running {
            workflow_state::update(&self.pool, task_id, update)
                .await?
                .ok_or(EngineError::NoWorkflowState)?;
            None
        } else {
            self.settle_group_with_failures(task_id, group, branches, update)
                .await?
        };
        // Appended after the commit, once per failed start, then the
        // settle's `Error` event (if it marked the task stuck).
        for (name, reason) in failures {
            self.record_branch_finished(
                task_id,
                group,
                name,
                entry_no,
                BranchEnd::Failed { reason },
            )
            .await;
        }
        if let Some(reason) = stuck_reason {
            self.append_settle_error(task_id, group, reason).await;
        }
        Ok(())
    }

    /// Applies a branch's end to its group. Takes the per-task lock; see
    /// [`Self::apply_branch_end`] for what it checks and writes. A database
    /// failure parks the task rather than being dropped.
    pub(super) async fn finish_branch(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        watch: &BranchWatch,
        branch: &str,
        session_id: &str,
        end: BranchOutcome,
    ) -> BranchApplied {
        let lock = self.lock_for_task(task_id).await;
        let _guard = lock.lock().await;
        let (applied, evict) = match self
            .apply_branch_end(task_id, definition, watch, branch, session_id, end)
            .await
        {
            Ok(done) => done,
            Err(err) => {
                tracing::error!(task_id, branch, %err, "could not record a branch's end");
                self.mark_stuck(
                    task_id,
                    &format!("stage '{branch}': could not record the branch's end: {err}"),
                    false,
                )
                .await;
                (BranchApplied::Parked(err.to_string()), true)
            }
        };
        if evict {
            self.evict_task_lock_if_unshared(task_id, &lock).await;
        }
        applied
    }

    /// The locked core of [`Self::finish_branch`]. Returns what happened and
    /// whether the task lock should be evicted.
    async fn apply_branch_end(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        watch: &BranchWatch,
        branch: &str,
        session_id: &str,
        end: BranchOutcome,
    ) -> Result<(BranchApplied, bool), EngineError> {
        let group = watch.group.as_str();
        let dropped = |why: String| {
            tracing::info!(
                task_id,
                group,
                branch,
                session_id,
                why,
                "branch end not applied"
            );
            Ok((BranchApplied::Dropped(why), false))
        };

        let Some(task) = tasks::get(&self.pool, task_id).await? else {
            return dropped("the task no longer exists".to_string());
        };
        if task.status != TASK_STATUS_OPEN {
            return dropped(format!("the task is {}", task.status));
        }
        let Some(state) = workflow_state::get(&self.pool, task_id).await? else {
            return dropped("the task has no workflow state".to_string());
        };
        if state.current_stage != group {
            return dropped(format!(
                "the task has left '{group}' for '{}'",
                state.current_stage
            ));
        }
        let Some(block) = block_for(&state.payload, group) else {
            return dropped(format!("no branch state for '{group}' in the payload"));
        };
        if block.get("entry").and_then(Value::as_i64) != Some(watch.entry) {
            return dropped(format!(
                "the branch belongs to entry {} of '{group}', which is no longer current",
                watch.entry
            ));
        }
        if branch_state(&state.payload, branch) != Some("running") {
            return dropped(format!("branch '{branch}' is no longer running"));
        }
        let current = sessions::get_current_for_stage(&self.pool, task_id, branch).await?;
        if current.as_ref().map(|s| s.id.as_str()) != Some(session_id) {
            return dropped(format!(
                "session {session_id} is no longer branch '{branch}'s current run"
            ));
        }

        let branch_ref = definition
            .branch(branch)
            .ok_or_else(|| EngineError::UnknownStage(branch.to_string()))?;
        let Some(StageKind::Parallel { branches }) = definition.stages.get(group).map(|d| &d.kind)
        else {
            return Err(EngineError::UnknownStage(group.to_string()));
        };

        // A result outside `results:` is a failure; its capture is dropped.
        let end = match end {
            BranchOutcome::Done {
                result,
                note,
                capture: _,
            } if !branch_ref.results.contains(&result) => BranchOutcome::Failed {
                reason: format!(
                    "stage '{branch}': result '{result}' is not one of its results [{}]{}",
                    branch_ref.results.join(", "),
                    note.map(|n| format!(" ({n})")).unwrap_or_default()
                ),
            },
            other => other,
        };

        let now = stamp(self.now());
        let mut payload = state.payload.clone();
        let (done, finished_end) = match &end {
            BranchOutcome::Done {
                result, capture, ..
            } => {
                end_branch(
                    &mut payload,
                    branch,
                    json!({ "state": "done", "result": result }),
                    &now,
                );
                if let Some(value) = capture {
                    merge_stage_capture(&mut payload, branch, value.clone());
                }
                mark_stage_finished(&mut payload, branch);
                (true, None)
            }
            BranchOutcome::Failed { reason } => {
                let session = sessions::get(&self.pool, session_id).await?;
                let (resumable, reason) = match self
                    .resumable_session(&task, definition, branch_ref.def, session.as_ref(), true)
                    .await
                {
                    Ok(verdict) => (verdict.is_ok(), reason.clone()),
                    Err(err) => {
                        tracing::error!(task_id, branch, %err, "could not tell whether a branch's session can be resumed");
                        (
                            false,
                            format!(
                                "{reason} (could not tell whether its session can be resumed: \
                                 {err})"
                            ),
                        )
                    }
                };
                fail_branch(&mut payload, branch, &reason, resumable, &now);
                (false, Some(reason))
            }
        };

        let running = branches
            .keys()
            .any(|name| branch_state(&payload, name) == Some("running"));
        let failed = branches
            .keys()
            .any(|name| branch_state(&payload, name) == Some("failed"));
        // What the timeline entry says, once the commit below has landed.
        let ended: Result<String, String> = match (&end, finished_end) {
            (BranchOutcome::Done { result, .. }, _) => Ok(result.clone()),
            (BranchOutcome::Failed { reason }, failed_reason) => {
                Err(failed_reason.unwrap_or_else(|| reason.clone()))
            }
        };

        if running || failed {
            let update = workflow_state::WorkflowStateUpdate {
                current_stage: state.current_stage,
                stage_kind: "parallel".to_string(),
                loop_counters: state.loop_counters,
                payload,
                enters_stage: false,
            };
            if running {
                workflow_state::update(&self.pool, task_id, update)
                    .await?
                    .ok_or(EngineError::NoWorkflowState)?;
                self.record_branch_end(task_id, group, branch, watch.entry, &ended)
                    .await;
                return Ok((BranchApplied::Recorded { done }, false));
            }
            let stuck_reason = self
                .settle_group_with_failures(task_id, group, branches, update)
                .await?;
            self.record_branch_end(task_id, group, branch, watch.entry, &ended)
                .await;
            if let Some(reason) = stuck_reason {
                self.append_settle_error(task_id, group, reason).await;
            }
            return Ok((BranchApplied::Recorded { done }, true));
        }

        // Every branch is done: leave the group through the same transition
        // a stage's `done` outcome takes.
        let mut state = state;
        state.payload = payload;
        let (next_stage, update) = self.compute_transition(definition, state, "done", None)?;
        let updated = workflow_state::update(&self.pool, task_id, update)
            .await?
            .ok_or(EngineError::NoWorkflowState)?;
        self.record_branch_end(task_id, group, branch, watch.entry, &ended)
            .await;
        let entered = self
            .enter_stage(
                task_id,
                definition,
                &next_stage,
                None,
                Some("done"),
                &updated.payload,
                None,
            )
            .await;
        if let Err(err) = entered {
            tracing::error!(
                task_id, group, %err,
                "task wedged: its parallel group completed but the next stage failed"
            );
            let blamed = self.stage_to_blame(task_id, group).await;
            let reason = if blamed == group {
                format!(
                    "stage '{group}': parallel stage completed but the transition failed: {err}"
                )
            } else {
                format!("stage '{blamed}': could not be entered after '{group}' completed: {err}")
            };
            self.mark_stuck(
                task_id,
                &reason,
                matches!(err, EngineError::Template { .. }),
            )
            .await;
            return Ok((BranchApplied::Recorded { done }, true));
        }
        let terminal = definition
            .stages
            .get(&next_stage)
            .is_some_and(|d| matches!(d.kind, StageKind::Terminal));
        Ok((BranchApplied::Recorded { done }, terminal))
    }
}
