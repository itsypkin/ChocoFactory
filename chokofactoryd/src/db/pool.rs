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
        sqlx::raw_sql(
            r#"
INSERT INTO projects (id, name, created_at)
    VALUES ('p1', 'demo', '2026-01-01T00:00:00Z');
INSERT INTO tasks (id, project_id, workflow_def, title, created_at, updated_at)
    VALUES ('t1', 'p1', 'chat', 'T', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, status, started_at)
    VALUES ('r1', 't1', 'chatting', 'chat', 'claude', 'sonnet', 'idle', '2026-01-01T00:00:00Z');
INSERT INTO events (id, task_run_id, seq, event_type, payload, created_at)
    VALUES ('e1', 'r1', 1, 'assistant_message', '{"text":"hi"}', '2026-01-01T00:00:01Z');
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

        let (id, task_id, task_run_id, payload): (String, String, Option<String>, String) =
            sqlx::query_as("SELECT id, task_id, task_run_id, payload FROM events")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(id, "e1");
        // Backfilled from the run it was appended against...
        assert_eq!(task_id, "t1");
        // ...without losing the run itself, or the payload.
        assert_eq!(task_run_id.as_deref(), Some("r1"));
        assert_eq!(payload, r#"{"text":"hi"}"#);

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
        let path = dir.join("chokofactory.db");
        connect(&path).await.unwrap();
        assert!(path.exists());
        // Reconnecting must not fail on already-applied migrations.
        connect(&path).await.unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chokofactoryd-test-{}-{}",
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
