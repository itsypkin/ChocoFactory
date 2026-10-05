-- #164: when the task's current stage was entered, for the dashboard's
-- "time in stage". Written by the same UPDATE that moves `current_stage`
-- (never by a second statement); a retry does not touch it.
--
-- Timestamps are stored by sqlx as RFC 3339 text in UTC with a fixed
-- `+00:00` suffix and 0/3/6/9 fractional digits. Because `+` sorts before
-- `.` and the digits, such strings sort chronologically as text, so MAX()
-- is safe here.
ALTER TABLE workflow_state ADD COLUMN stage_entered_at TEXT;

UPDATE workflow_state
SET stage_entered_at = (
    SELECT MAX(e.created_at)
    FROM events e
    WHERE e.task_id = workflow_state.task_id
      AND e.event_type = 'stage_entered'
      AND json_extract(e.payload, '$.stage') = workflow_state.current_stage
      -- a retry re-runs the stage and also writes a `stage_entered` event
      -- (outcome 'retry'), but live code leaves stage_entered_at alone then
      AND json_extract(e.payload, '$.outcome') IS NOT 'retry'
);
