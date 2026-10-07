-- #175: the kind of the task's current stage ('agent_turn', 'shell', 'poll',
-- 'human_gate' or 'terminal'), so `GET /tasks` can say a task is waiting on a
-- person without loading its workflow file. Written by the same UPDATE that
-- moves `current_stage`.
--
-- Rows from before this migration stay NULL until a transition, a retry or
-- the startup sweep sets them, because stage kinds live in workflow files,
-- not in the database.
ALTER TABLE workflow_state ADD COLUMN stage_kind TEXT;
