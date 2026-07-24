-- Distinguishes *why* a task_run reached its final status, when that
-- matters beyond the status itself: a clean exit into `idle` can mean the
-- turn actually finished, or that the idle reaper force-closed stdin on a
-- turn that was merely stalled (session.rs's `drain_session`). Both land on
-- the same `status`, so the workflow engine's turn-completion watcher
-- (engine.rs) needs `end_reason` to tell them apart before auto-advancing.
-- NULL for every other case (still active, or exited via non-zero/crash).
ALTER TABLE task_runs ADD COLUMN end_reason TEXT;
