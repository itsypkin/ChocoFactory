CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects (id),
    parent_task_id TEXT REFERENCES tasks (id),
    workflow_def TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    config TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_tasks_project_id ON tasks (project_id);
CREATE INDEX idx_tasks_parent_task_id ON tasks (parent_task_id);
CREATE INDEX idx_tasks_status ON tasks (status);

CREATE TABLE task_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id),
    stage TEXT NOT NULL,
    role TEXT NOT NULL,
    cli_adapter TEXT NOT NULL,
    model TEXT NOT NULL,
    session_id TEXT,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE INDEX idx_task_runs_task_id ON task_runs (task_id);
CREATE INDEX idx_task_runs_status ON task_runs (status);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    task_run_id TEXT NOT NULL REFERENCES task_runs (id),
    seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (task_run_id, seq)
);

CREATE INDEX idx_events_task_run_id ON events (task_run_id);
CREATE INDEX idx_events_created_at ON events (created_at);

CREATE TABLE workflow_state (
    task_id TEXT PRIMARY KEY REFERENCES tasks (id),
    current_stage TEXT NOT NULL,
    loop_counters TEXT NOT NULL DEFAULT '{}',
    stage_history TEXT NOT NULL DEFAULT '[]',
    payload TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL
);
