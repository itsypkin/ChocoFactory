use super::*;

impl WorkflowEngine {
    /// Spawns a detached runner and registers its handle so `cancel_task`
    /// (and shutdown) can abort it. `make` receives the runner id the
    /// future must pass to [`Self::finish_runner`] when it ends.
    ///
    /// The flag check, the spawn and the registration all happen under the
    /// `detached_runners` lock, so a runner is either refused (shutdown has
    /// begun) or visible to `abort_all_detached_runners`; there is no
    /// half-registered slot. Returns `false`, spawning nothing, once
    /// shutdown has begun. The caller leaves the task where it is, and the
    /// next start's park and poll sweeps recover it.
    ///
    /// Callers hold the task's `task_locks` entry, so `cancel_task` cannot
    /// interleave.
    pub(super) fn spawn_registered_runner<F>(
        &self,
        task_id: &str,
        make: impl FnOnce(u64) -> F,
    ) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut runners = self
            .detached_runners
            .lock()
            .expect("detached_runners mutex poisoned");
        if self.runners_stopping.load(Ordering::SeqCst) {
            return false;
        }
        let id = self.next_runner_id.fetch_add(1, Ordering::Relaxed);
        let handle = tokio::spawn(make(id));
        runners
            .entry(task_id.to_string())
            .or_default()
            .insert(id, Some(handle));
        true
    }

    /// Drops a finished runner's slot, and the task's whole entry once its
    /// last runner is gone, so the map doesn't grow without bound for
    /// tasks nobody ever cancels.
    pub(super) fn finish_runner(&self, task_id: &str, id: u64) {
        let mut runners = self
            .detached_runners
            .lock()
            .expect("detached_runners mutex poisoned");
        if let Some(task) = runners.get_mut(task_id) {
            task.remove(&id);
            if task.is_empty() {
                runners.remove(task_id);
            }
        }
    }

    /// Aborts every detached runner of every task and waits until each
    /// future is dropped (so each process-group guard has SIGKILLed its
    /// group). Used at shutdown, before the daemon lock is released.
    pub async fn abort_all_detached_runners(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut runners = self
                .detached_runners
                .lock()
                .expect("detached_runners mutex poisoned");
            self.runners_stopping.store(true, Ordering::SeqCst);
            runners
                .drain()
                .flat_map(|(_, task)| task.into_values().flatten())
                .collect()
        };
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Aborts every detached `shell`/`poll` runner in flight for `task_id`
    /// (#69), killing the command each one is running.
    ///
    /// Abort drops the runner's future at its current await point, which
    /// drops `shell::run`'s `ProcessGroup` guard, whose `Drop` SIGKILLs the
    /// command's whole process group — the same teardown a `timeout:`
    /// already relies on. A runner that has already finished aborts
    /// harmlessly.
    ///
    /// The entry is removed wholesale: an aborted runner never reaches its
    /// own `finish_runner` call, so nothing else would clean it up.
    pub(super) async fn abort_detached_runners(&self, task_id: &str) {
        let handles: Vec<JoinHandle<()>> = {
            let mut runners = self
                .detached_runners
                .lock()
                .expect("detached_runners mutex poisoned");
            runners
                .remove(task_id)
                .map(|task| task.into_values().flatten().collect())
                .unwrap_or_default()
        };
        if handles.is_empty() {
            return;
        }
        tracing::info!(
            task_id,
            runners = handles.len(),
            "cancelling task: aborting its in-flight shell/poll runners"
        );
        // `abort` only *schedules* the task to be dropped, and the SIGKILL
        // happens in that drop — so awaiting each handle afterwards is what
        // makes this deterministic rather than hopeful. A cancelled
        // `JoinHandle` resolves once the future has actually been dropped,
        // which is precisely the point the command's process group has been
        // killed. Without the await, the caller could go on to
        // `git worktree remove --force` the directory those commands are
        // still running in.
        for handle in handles {
            handle.abort();
            // The expected outcome is `Err(JoinError::Cancelled)`. `Ok` is
            // a runner that finished on its own just before the abort, and
            // a panicked runner is already reported by its own task — so
            // neither is worth handling here, only waiting for.
            let _ = handle.await;
        }
    }

    /// Whether `task_id` has been cancelled (#69).
    ///
    /// Advisory helper for long-running detached work — see
    /// `run_poll_stage`. A `true` here is authoritative (the column is only
    /// ever set one way), but a `false` can go stale the moment it's read,
    /// so this must never be the *only* thing standing between a cancelled
    /// task and a transition. `advance_from_stage`'s check, taken inside
    /// the per-task lock, is what actually enforces it.
    ///
    /// A failed read answers `false` — "keep going" — matching
    /// `still_in_stage`'s handling of the same case: a transient DB error
    /// should not silently abandon a task's in-flight work.
    pub(super) async fn is_cancelled(&self, task_id: &str) -> bool {
        match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task.status == TASK_STATUS_CANCELLED,
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, %err,
                    "could not check whether a task was cancelled; assuming it was not"
                );
                false
            }
        }
    }

    /// Records what a `shell` or `poll` stage's command did.
    ///
    /// Best-effort, like every other event append in this module: the
    /// command has already run, so failing to record it can't be undone by
    /// refusing to transition — and refusing would strand the task in a
    /// stage whose work is complete.
    pub(super) async fn append_command_event(&self, task_id: &str, payload: Value) {
        match events::append_for_task(&self.pool, task_id, EventType::ShellOutput, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, %err,
                "failed to record stage command output event"
            ),
        }
    }

    /// Whether a detached runner is registered for `task_id`. Only a
    /// meaningful ownership answer while holding the task's lock — see the
    /// invariant on `detached_runners`.
    pub(super) fn has_detached_runner(&self, task_id: &str) -> bool {
        self.detached_runners
            .lock()
            .expect("detached_runners mutex poisoned")
            .contains_key(task_id)
    }
}
