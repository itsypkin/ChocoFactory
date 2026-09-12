-- #83: drops the delegation tag. `tasks.parent_task_id` was the storage
-- half of §6.2's `choco task create --parent-task <id>` (#10), which #19
-- closed without ever making usable: an agent inside a task is never told
-- its own task id, so nothing could supply the value. The column, its
-- index, `Task.parent_task_id`, and the CLI/API surface all go together --
-- leaving the column would keep an always-null `parent_task_id` in every
-- task payload, which is the most visible part of the feature being
-- removed.
--
-- Two ALTERs rather than the create-copy-drop-rename rebuild 0003 used on
-- `events`, and the difference matters:
--
--   * `tasks` is the *target* of foreign keys from `task_runs.task_id`,
--     `workflow_state.task_id`, and `events.task_id`. `DROP TABLE tasks`
--     fails with `FOREIGN KEY constraint failed` while any of those rows
--     exist -- and it cannot be worked around here, because `PRAGMA
--     foreign_keys` is a no-op inside a transaction and sqlx runs every
--     migration in one. `events` had no dependants, so 0003 was free to
--     rebuild.
--   * `ALTER TABLE ... DROP COLUMN` is permitted despite the column's own
--     `REFERENCES tasks (id)` clause, because that constraint is defined on
--     the dropped column and departs with it. The index is not optional
--     though -- SQLite refuses to drop an indexed column, hence the
--     explicit DROP INDEX first.
--
-- Existing rows are preserved; only the column is lost. Any delegation
-- links a user happens to have recorded are discarded, which is the point.

DROP INDEX idx_tasks_parent_task_id;

ALTER TABLE tasks DROP COLUMN parent_task_id;
