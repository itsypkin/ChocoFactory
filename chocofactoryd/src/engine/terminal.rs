use super::*;

impl WorkflowEngine {
    /// Removes `task_id`'s worktree, best-effort — logged loudly on
    /// failure, never propagated (§5.5 Q7, issue #58).
    ///
    /// Called from `dispatch_stage`'s `StageKind::Terminal` arm and from
    /// `cancel_task` (#69) — §5.5's "removed on reaching `done` (or task
    /// cancellation)". Both call it only *after* the task is already
    /// durably `closed`/`cancelled`, so there is nothing left here that a
    /// returned error could still roll back. `worktree::remove` is
    /// idempotent, so the two paths racing each other is safe.
    ///
    /// Callers are responsible for having checked that a worktree should
    /// exist at all — the terminal arm via `definition.worktree`,
    /// `cancel_task` via `worktree_snapshot` — since this logs an error
    /// when a task it is asked to clean up carries no snapshot.
    pub(super) async fn remove_worktree(self: &Arc<Self>, task_id: &str) -> bool {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                tracing::error!(task_id, "task disappeared before worktree removal");
                return false;
            }
            Err(err) => {
                tracing::error!(task_id, %err, "failed to load task for worktree removal");
                return false;
            }
        };
        let Some((repo, project)) = worktree_snapshot(&task) else {
            tracing::error!(
                task_id,
                "worktree-enabled task has no worktree_repo/worktree_project snapshot to remove"
            );
            return false;
        };
        match worktree::remove(&repo, project, &task.id).await {
            Ok(()) => {
                tracing::info!(task_id, "worktree removed (task closed)");
                true
            }
            Err(err) => {
                tracing::error!(
                    task_id, %err,
                    "failed to remove worktree after entering terminal stage"
                );
                false
            }
        }
    }

    /// Deletes the task's local branch after its worktree is gone (#102),
    /// best-effort like [`Self::remove_worktree`]: never propagated, but
    /// every failure is logged at `error` *and* put on the timeline.
    ///
    /// Lives in the engine rather than in a workflow stage because a
    /// `worktree: true` workflow's shell stages run inside the worktree, and
    /// git refuses to delete a branch that is checked out there; the
    /// worktree is only removed by the terminal stage, after which no stage
    /// runs. `only_if_safe` is `done`'s rule (delete only if the tip is on a
    /// remote-tracking ref); cancel passes `false`.
    ///
    /// The tip goes on the timeline *before* `git branch -D` runs
    /// (`worktree::delete_branch`'s hook), so a deleted branch is
    /// recoverable from the recorded SHA until `git gc` prunes it (about
    /// two weeks); if the note can't be written the branch is kept.
    pub(super) async fn cleanup_branch(self: &Arc<Self>, task_id: &str, only_if_safe: bool) {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                tracing::error!(task_id, "task disappeared before branch cleanup");
                return;
            }
            Err(err) => {
                tracing::error!(task_id, %err, "failed to load task for branch cleanup");
                return;
            }
        };
        // No snapshot, no worktree, so no branch of ours.
        let Some((repo, project)) = worktree_snapshot(&task) else {
            return;
        };
        let branch = worktree::branch_name(task_id);
        let pool_note = |sha: String| {
            let this = Arc::clone(self);
            let branch = branch.clone();
            let task_id = task_id.to_string();
            async move {
                let recorded = this
                    .record_branch_note(
                        &task_id,
                        json!({
                            "branch": branch,
                            "sha": sha,
                            "action": "deleting",
                            "message": format!("deleting branch {branch} at {sha}"),
                        }),
                    )
                    .await;
                if !recorded {
                    tracing::error!(
                        task_id = %task_id, %branch, %sha,
                        "could not record the branch tip; the branch will not be deleted"
                    );
                }
                recorded
            }
        };
        match worktree::delete_branch(&repo, project, task_id, only_if_safe, pool_note).await {
            Ok(worktree::BranchOutcome::Deleted { sha }) => {
                tracing::info!(task_id, %branch, %sha, "task branch deleted");
            }
            Ok(worktree::BranchOutcome::Absent) => {
                tracing::info!(task_id, %branch, "task branch already absent");
            }
            Ok(worktree::BranchOutcome::Kept { sha, reason }) => {
                tracing::info!(task_id, %branch, %sha, %reason, "task branch kept");
                self.record_branch_note(
                    task_id,
                    json!({
                        "branch": branch,
                        "sha": sha,
                        "action": "kept",
                        "reason": reason,
                        "message": format!("kept branch {branch} at {sha}: {reason}"),
                    }),
                )
                .await;
            }
            Err(err) => {
                tracing::error!(task_id, %branch, %err, "failed to delete task branch");
                self.record_branch_note(
                    task_id,
                    json!({
                        "branch": branch,
                        "action": "delete_failed",
                        "error": err.to_string(),
                        "message": format!("could not delete branch {branch}: {err}"),
                    }),
                )
                .await;
            }
        }
    }

    /// The worktree could not be removed, so the branch was not touched:
    /// logged at `error` and put on the timeline so it is visible why the
    /// branch is still there.
    pub(super) async fn note_branch_left_in_place(&self, task_id: &str) {
        tracing::error!(
            task_id,
            "worktree was not removed, so the task branch was left in place"
        );
        let branch = worktree::branch_name(task_id);
        self.record_branch_note(
            task_id,
            json!({
                "branch": branch,
                "action": "kept",
                "reason": "worktree removal failed",
                "message": format!("kept branch {branch}: worktree removal failed"),
            }),
        )
        .await;
    }

    /// Appends one `branch_cleanup` event, best-effort like every other
    /// timeline write in this file: a failure is logged at `error`.
    /// Returns whether the event was recorded.
    async fn record_branch_note(&self, task_id: &str, payload: Value) -> bool {
        match events::append_for_task(&self.pool, task_id, EventType::BranchCleanup, payload).await
        {
            Ok(_) => {
                self.events_notify.notify_waiters();
                true
            }
            Err(err) => {
                tracing::error!(
                    task_id, %err,
                    "failed to record a branch-cleanup event"
                );
                false
            }
        }
    }
}
