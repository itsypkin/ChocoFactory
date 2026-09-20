-- #48: `task_run` never meant "one run of the task" — a row is the record
-- of one agent session, scoped to a single `agent_turn` stage. Only the
-- `AgentTurn` arm of `enter_stage` ever creates one, and a follow-up
-- message reuses the existing row rather than making a new one.
-- `SessionManager.sessions` in `session.rs` is already keyed by that row's
-- id, so the live map and the durable table were the same concept under
-- two names. This migration renames the table (and the code around it) to
-- match.
--
-- `events.task_run_id` is a foreign key into `task_runs`, and SQLite can't
-- rename a table out from under an FK, so this is drop-and-recreate rather
-- than a series of ALTERs — the same constraint 0003 hit renaming `events`
-- itself.
--
-- This project has no users and no production data yet: every local
-- database's `task_runs`/`events` rows are test data, and losing them is
-- an acceptable, deliberate cost of not needing a rebuild-and-copy here.
-- `tasks`, `projects` and `workflow_state` are untouched, so a task
-- created before this migration keeps its row and simply has no sessions
-- or events afterward. Anyone with a local database loses their task
-- history the next time the daemon starts.
--
-- While renaming the table, this also resolves the name collision the
-- issue called out: the old `task_runs.session_id` held the *CLI
-- adapter's* session identifier (written by `set_session_id`, used for
-- `--resume`), which read as if it were the row's own id once the row
-- itself is called a session. It becomes `adapter_session_id` here so the
-- two can't be confused.

DROP TABLE events;
DROP TABLE task_runs;

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id),
    stage TEXT NOT NULL,
    role TEXT NOT NULL,
    cli_adapter TEXT NOT NULL,
    model TEXT NOT NULL,
    adapter_session_id TEXT,
    status TEXT NOT NULL,
    end_reason TEXT,
    resumed_from TEXT REFERENCES sessions (id),
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE INDEX idx_sessions_task_id ON sessions (task_id);
CREATE INDEX idx_sessions_status ON sessions (status);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id),
    session_id TEXT REFERENCES sessions (id),
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_events_task_id_created_at ON events (task_id, created_at, id);
CREATE INDEX idx_events_session_id_created_at ON events (session_id, created_at, id);
CREATE INDEX idx_events_created_at ON events (created_at);
