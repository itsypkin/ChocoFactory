use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens (creating if needed) the SQLite file at `path` and applies any
/// pending migrations.
pub async fn connect(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// A private, in-memory database for tests. `max_connections(1)` keeps every
/// borrow from the pool on the same SQLite connection, since each connection
/// to `sqlite::memory:` is otherwise its own separate empty database.
pub async fn connect_in_memory() -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_cleanly() {
        let pool = connect_in_memory().await.unwrap();
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let names: Vec<String> = tables.into_iter().map(|(n,)| n).collect();
        for expected in ["projects", "tasks", "task_runs", "events", "workflow_state"] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing table {expected}"
            );
        }
    }

    /// 0003 rebuilds `events` and backfills the new `task_id` from
    /// `task_runs`. Every other test migrates an *empty* database, so this
    /// is the only place the backfill actually moves data — and getting it
    /// wrong would silently orphan a user's entire event history.
    #[tokio::test]
    async fn stage_transition_migration_backfills_and_preserves_existing_events() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        // Build the pre-0003 schema and put a row in it, rather than
        // running the migrator (which would apply 0003 immediately).
        sqlx::raw_sql(include_str!("../../migrations/0001_init.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/0002_task_run_end_reason.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        // Two tasks, each with its own run, plus a third event appended
        // back to the *first* run after the second one exists. With only a
        // single run in the table any expression in the `task_id` position
        // would produce the right answer — a cross join, a wrong join key,
        // or an uncorrelated subquery all included — so the mapping has to
        // be ambiguous here for the assertions below to mean anything.
        sqlx::raw_sql(
            r#"
INSERT INTO projects (id, name, created_at)
    VALUES ('p1', 'demo', '2026-01-01T00:00:00Z');
INSERT INTO tasks (id, project_id, workflow_def, title, created_at, updated_at)
    VALUES ('t1', 'p1', 'chat', 'T1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
           ('t2', 'p1', 'chat', 'T2', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, status, started_at)
    VALUES ('r1', 't1', 'chatting', 'chat', 'claude', 'sonnet', 'idle', '2026-01-01T00:00:00Z'),
           ('r2', 't2', 'chatting', 'chat', 'claude', 'sonnet', 'idle', '2026-01-01T00:00:00Z');
INSERT INTO events (id, task_run_id, seq, event_type, payload, created_at)
    VALUES ('e1', 'r1', 1, 'assistant_message', '{"text":"hi"}',    '2026-01-01T00:00:01Z'),
           ('e2', 'r2', 1, 'assistant_message', '{"text":"other"}', '2026-01-01T00:00:02Z'),
           ('e3', 'r1', 2, 'tool_call',         '{"tool":"bash"}',  '2026-01-01T00:00:03Z');
INSERT INTO workflow_state (task_id, current_stage, loop_counters, stage_history, payload, updated_at)
    VALUES ('t1', 'chatting', '{}', '["start"]', '{}', '2026-01-01T00:00:00Z');
"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../../migrations/0003_stage_transition_events.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        // Every row keeps its own run, and lands on *that run's* task.
        let rows: Vec<(String, String, Option<String>, String)> =
            sqlx::query_as("SELECT id, task_id, task_run_id, payload FROM events ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "e1".to_string(),
                    "t1".to_string(),
                    Some("r1".to_string()),
                    r#"{"text":"hi"}"#.to_string()
                ),
                (
                    "e2".to_string(),
                    "t2".to_string(),
                    Some("r2".to_string()),
                    r#"{"text":"other"}"#.to_string()
                ),
                (
                    "e3".to_string(),
                    "t1".to_string(),
                    Some("r1".to_string()),
                    r#"{"tool":"bash"}"#.to_string()
                ),
            ]
        );

        // The FK survived the rebuild + RENAME: a bogus task is rejected...
        let bad = sqlx::query(
            "INSERT INTO events (id, task_id, task_run_id, event_type, payload, created_at)
             VALUES ('x', 'no-such-task', NULL, 'stage_entered', '{}', '2026-01-01T00:00:04Z')",
        )
        .execute(&pool)
        .await;
        assert!(bad.is_err(), "events.task_id FK should still be enforced");

        // ...while a session-less row (the whole point of 0003) is accepted.
        sqlx::query(
            "INSERT INTO events (id, task_id, task_run_id, event_type, payload, created_at)
             VALUES ('s1', 't1', NULL, 'stage_entered', '{}', '2026-01-01T00:00:05Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let indexes: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master
             WHERE type = 'index' AND tbl_name = 'events' AND name NOT LIKE 'sqlite_autoindex%'
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let indexes: Vec<String> = indexes.into_iter().map(|(n,)| n).collect();
        for expected in [
            "idx_events_created_at",
            "idx_events_task_id_created_at",
            "idx_events_task_run_id_created_at",
        ] {
            assert!(
                indexes.iter().any(|n| n == expected),
                "missing index {expected}, have {indexes:?}"
            );
        }

        // `stage_history` is gone; the rest of the row survives.
        let (current_stage,): (String,) =
            sqlx::query_as("SELECT current_stage FROM workflow_state WHERE task_id = 't1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(current_stage, "chatting");
        assert!(
            sqlx::query("SELECT stage_history FROM workflow_state")
                .fetch_optional(&pool)
                .await
                .is_err(),
            "stage_history should no longer exist"
        );
    }

    #[tokio::test]
    async fn connect_creates_file_and_is_idempotent() {
        let dir = tempdir();
        let path = dir.join("chocofactory.db");
        connect(&path).await.unwrap();
        assert!(path.exists());
        // Reconnecting must not fail on already-applied migrations.
        connect(&path).await.unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chocofactoryd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
