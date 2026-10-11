-- Agent turns waiting for a slot, first in first out by `id`.
CREATE TABLE turn_queue (
    id INTEGER PRIMARY KEY,                -- queue order; a new row is always after every existing one
    task_id TEXT NOT NULL REFERENCES tasks (id),
    stage TEXT NOT NULL,                   -- the agent stage, or the branch of a parallel group
    group_stage TEXT,                      -- that group; NULL for a top-level stage
    group_entry INTEGER,                   -- the group's entry number; NULL with group_stage
    resume_session_id TEXT REFERENCES sessions (id),  -- the session a retry chose to resume
    queued_at TEXT NOT NULL,
    UNIQUE (task_id, stage)                -- one waiting turn per stage or branch
);
CREATE INDEX idx_turn_queue_task_id ON turn_queue (task_id);

-- A project's own agent cap. NULL: the project shares the server's cap.
ALTER TABLE projects ADD COLUMN max_agents INTEGER;
