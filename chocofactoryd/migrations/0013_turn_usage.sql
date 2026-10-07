-- What each agent turn used (cost, tokens, time), one row per `result` line.
-- Never pruned: retention only ages out `events`, so totals survive it, and
-- nothing here references `events`.
CREATE TABLE turn_usage (
    id INTEGER PRIMARY KEY,              -- insertion order defines "previous"
    task_id TEXT NOT NULL REFERENCES tasks (id),
    session_id TEXT NOT NULL REFERENCES sessions (id),
    recorded_at TEXT NOT NULL,
    billing TEXT NOT NULL,               -- 'subscription' | 'api_key' | 'unknown'
    counting TEXT NOT NULL,              -- 'cumulative' | 'per_turn'
    reported_cost_usd REAL,              -- as the CLI reported it
    cost_usd REAL,                       -- this turn's own cost
    input_tokens INTEGER, output_tokens INTEGER,
    cache_read_tokens INTEGER, cache_write_tokens INTEGER,
    duration_ms INTEGER, model_turns INTEGER,
    reported_models TEXT,                -- JSON, as reported
    models TEXT                          -- JSON, this turn's own
);
CREATE INDEX idx_turn_usage_task_id ON turn_usage (task_id);
CREATE INDEX idx_turn_usage_session_id ON turn_usage (session_id, id);

-- The nth time the task entered the session's stage (NULL for sessions
-- created before this column existed).
ALTER TABLE sessions ADD COLUMN lap INTEGER;
