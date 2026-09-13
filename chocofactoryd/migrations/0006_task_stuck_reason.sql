-- X-4 (#61): a task the engine could not move forward on its own — a
-- stage whose outcome has no `on:` edge, a transition that failed, an
-- agent turn whose session never started, a run the idle reaper
-- force-closed, or a turn whose process exited without completing —
-- used to stay `open` forever, indistinguishable from a healthy task
-- except in the daemon's log. `stuck_reason` is the human-readable
-- explanation that goes with `tasks.status = 'stuck'`; NULL for every
-- other status and for every existing row.
ALTER TABLE tasks ADD COLUMN stuck_reason TEXT;
