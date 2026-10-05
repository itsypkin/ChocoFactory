-- #102: `choco task cancel --keep` stops a task's agents but leaves its
-- worktree and branch in place for a person to take over. `kept_work`
-- records that decision. It is written by the same UPDATE that sets
-- `status = 'cancelled'`, never by a second statement. 0 for every
-- existing row and every task not cancelled with --keep.
ALTER TABLE tasks ADD COLUMN kept_work INTEGER NOT NULL DEFAULT 0;
