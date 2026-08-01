-- X-3 (#44): stage transitions become first-class entries in the events
-- timeline. `events` stops being "the agent session log" and becomes "the
-- task timeline", where session attribution is optional. Three consequences
-- the old schema can't express:
--
-- 1. `task_id` is denormalized onto `events`. A stage transition belongs to
--    a *task*, not to an agent session: `human_gate` and `terminal` stages
--    never create a `task_runs` row at all (see `WorkflowEngine::
--    enter_stage`), so there is nothing to join through. Every read path
--    that previously reached task identity via `JOIN task_runs tr ON
--    tr.id = e.task_run_id` would silently drop these rows.
-- 2. `task_run_id` becomes nullable, for the same reason.
-- 3. `seq` is dropped entirely. It only ever ordered events *within* one
--    task_run, and those are appended sequentially by a single drain loop
--    with a full INSERT round trip between them, while `created_at` carries
--    microsecond resolution -- so it bought no ordering safety on the path
--    it covered. (Where a same-microsecond collision is plausible at all --
--    two concurrent producers -- `seq` could not have helped, since a stage
--    transition has no `seq` to compare against.) Every read path now uses
--    one rule: `ORDER BY created_at, id`. The cost is losing gap detection,
--    accepted deliberately.
--
-- SQLite can neither add a NOT NULL column without a constant default (the
-- backfilled value is per-row) nor relax an existing NOT NULL, so this is a
-- full table rebuild rather than a series of ALTERs.

CREATE TABLE events_new (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id),
    task_run_id TEXT REFERENCES task_runs (id),
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Every pre-existing row came from a session, so `task_id` backfills from
-- the run it was appended against and none of them are dropped by the join.
INSERT INTO events_new (id, task_id, task_run_id, event_type, payload, created_at)
SELECT e.id, tr.task_id, e.task_run_id, e.event_type, e.payload, e.created_at
FROM events e
JOIN task_runs tr ON tr.id = e.task_run_id;

DROP TABLE events;

ALTER TABLE events_new RENAME TO events;

-- The task timeline (`list_for_task`/`_after`/`_page`) -- re-run on every
-- live-WS wakeup, so it covers the ORDER BY as well as the filter.
CREATE INDEX idx_events_task_id_created_at ON events (task_id, created_at, id);
-- One session's slice of that timeline (`list_for_task_run`), which now
-- orders by `(created_at, id)` too rather than by the departed `seq`.
CREATE INDEX idx_events_task_run_id_created_at ON events (task_run_id, created_at, id);
-- The retention job prunes by age across all tasks (§4.4).
CREATE INDEX idx_events_created_at ON events (created_at);

-- Superseded by filtering the timeline for `stage_entered`, which carries
-- strictly more information (a timestamp, and the outcome that caused the
-- transition) and -- unlike this column, which only ever recorded stages on
-- the way *out* -- also records the task's entry stage.
ALTER TABLE workflow_state DROP COLUMN stage_history;
