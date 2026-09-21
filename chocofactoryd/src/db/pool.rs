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
        for expected in ["projects", "tasks", "sessions", "events", "workflow_state"] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing table {expected}"
            );
        }
        // #48: `task_runs` is gone, renamed to `sessions` — and along with
        // it, the old `task_runs.session_id` (the CLI adapter's own session
        // id) becomes `sessions.adapter_session_id`, freeing `session_id` for
        // `events`' foreign key into the renamed table.
        assert!(
            !names.iter().any(|n| n == "task_runs"),
            "task_runs should no longer exist, have {names:?}"
        );

        let session_columns: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('sessions')")
                .fetch_all(&pool)
                .await
                .unwrap();
        let session_columns: Vec<String> = session_columns.into_iter().map(|(n,)| n).collect();
        assert!(
            session_columns.iter().any(|n| n == "adapter_session_id"),
            "sessions should have adapter_session_id, have {session_columns:?}"
        );
        assert!(
            !session_columns.iter().any(|n| n == "session_id"),
            "sessions should not have its own session_id column, have {session_columns:?}"
        );

        let event_columns: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('events')")
                .fetch_all(&pool)
                .await
                .unwrap();
        let event_columns: Vec<String> = event_columns.into_iter().map(|(n,)| n).collect();
        assert!(
            event_columns.iter().any(|n| n == "session_id"),
            "events should have session_id, have {event_columns:?}"
        );
        assert!(
            !event_columns.iter().any(|n| n == "task_run_id"),
            "events should no longer have task_run_id, have {event_columns:?}"
        );
    }

    /// #48: an existing local database (one that predates the `sessions`
    /// rename) must still upgrade cleanly. This deliberately doesn't assert
    /// that its `task_runs`/`events` rows survive — 0009's header explains
    /// why they don't — only that applying the migration against a database
    /// that already has data doesn't fail.
    ///
    /// Goes through `MIGRATOR.run`, the same path `connect`/
    /// `connect_in_memory` use at real startup — inside a transaction, with
    /// `PRAGMA foreign_keys = ON` already set on the connection, and rows
    /// present — rather than applying 0009's SQL directly. 0001-0008 are
    /// still applied by hand (below) to build the fixture's starting
    /// schema, but each is also recorded in `_sqlx_migrations` with its
    /// real checksum (read straight off `MIGRATOR`, so this can't drift
    /// from the migration files themselves) so the migrator treats them as
    /// already applied and only actually *runs* 0009.
    #[tokio::test]
    async fn rename_to_sessions_migrates_a_pre_existing_database_cleanly() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        // Every migration up to (not including) 0009, so the fixture below
        // is written against the schema a real pre-#48 database would have.
        for sql in [
            include_str!("../../migrations/0001_init.sql"),
            include_str!("../../migrations/0002_task_run_end_reason.sql"),
            include_str!("../../migrations/0003_stage_transition_events.sql"),
            include_str!("../../migrations/0004_task_worktree_snapshot.sql"),
            include_str!("../../migrations/0005_drop_task_parent_task_id.sql"),
            include_str!("../../migrations/0006_task_stuck_reason.sql"),
            include_str!("../../migrations/0007_task_run_resumed_from.sql"),
            include_str!("../../migrations/0008_project_repo_and_task_workflow_file.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }

        // A task with two chained runs — r2 resumed r1's interrupted
        // session, via the self-FK 0007 added — and an event against each.
        // The chain matters: it's what exercises `DROP TABLE task_runs`
        // against a `resumed_from` self-reference rather than only the
        // no-chain case, and it's exactly the kind of row 0009's header
        // says is discarded rather than carried forward.
        sqlx::raw_sql(
            r#"
INSERT INTO projects (id, name, created_at)
    VALUES ('p1', 'demo', '2026-01-01T00:00:00Z');
INSERT INTO tasks (id, project_id, workflow_def, title, created_at, updated_at)
    VALUES ('t1', 'p1', 'chat', 'T1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, session_id, status, started_at)
    VALUES ('r1', 't1', 'chatting', 'chat', 'claude', 'sonnet', 'sess-1', 'idle', '2026-01-01T00:00:00Z');
INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, session_id, status, resumed_from, started_at)
    VALUES ('r2', 't1', 'chatting', 'chat', 'claude', 'sonnet', 'sess-2', 'idle', 'r1', '2026-01-01T00:00:02Z');
INSERT INTO events (id, task_id, task_run_id, event_type, payload, created_at)
    VALUES ('e1', 't1', 'r1', 'assistant_message', '{"text":"hi"}', '2026-01-01T00:00:01Z'),
           ('e2', 't1', 'r2', 'assistant_message', '{"text":"again"}', '2026-01-01T00:00:03Z');
"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Record 0001-0008 as already applied, with their real checksums,
        // so `MIGRATOR.run` below skips re-running SQL that already ran
        // above and applies only 0009 — through its normal machinery.
        // `_sqlx_migrations` itself doesn't exist yet outside of
        // `MIGRATOR.run` (which is what creates it, normally); its schema
        // is created here up front, matching what sqlx's own migrator
        // creates, purely so these bookkeeping rows have somewhere to go.
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Found by description rather than hardcoded as `< 9`, so a later
        // migration (0010+) landing after this one doesn't silently widen
        // what gets seeded here — and, with it, what `MIGRATOR.run` below
        // actually exercises.
        let rename_migration_version = MIGRATOR
            .iter()
            .find(|m| &*m.description == "rename task runs to sessions")
            .expect("migration 0009_rename_task_runs_to_sessions must exist")
            .version;
        for migration in MIGRATOR
            .iter()
            .filter(|m| m.version < rename_migration_version)
        {
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                 VALUES (?1, ?2, TRUE, ?3, -1)",
            )
            .bind(migration.version)
            .bind(&*migration.description)
            .bind(&*migration.checksum)
            .execute(&pool)
            .await
            .unwrap();
        }

        // The point of the test: applying 0009 against a database that
        // already has data succeeds rather than failing on the
        // drop-and-recreate.
        MIGRATOR.run(&pool).await.unwrap();

        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let names: Vec<String> = tables.into_iter().map(|(n,)| n).collect();
        assert!(names.iter().any(|n| n == "sessions"));
        assert!(names.iter().any(|n| n == "events"));
        assert!(!names.iter().any(|n| n == "task_runs"));

        // The task itself survives (0009 never touches `tasks`); its old
        // sessions and events do not.
        let (task_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(task_count, 1);
        let (session_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(session_count, 0);
        let (event_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(event_count, 0);

        // The rebuilt `events` table still enforces its FKs: a bogus task
        // is rejected...
        let bad_task = sqlx::query(
            "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
             VALUES ('x', 'no-such-task', NULL, 'stage_entered', '{}', '2026-01-01T00:00:04Z')",
        )
        .execute(&pool)
        .await;
        assert!(bad_task.is_err(), "events.task_id FK should be enforced");

        // ...a bogus session is rejected...
        let bad_session = sqlx::query(
            "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
             VALUES ('y', 't1', 'no-such-session', 'assistant_message', '{}', '2026-01-01T00:00:05Z')",
        )
        .execute(&pool)
        .await;
        assert!(
            bad_session.is_err(),
            "events.session_id FK should be enforced"
        );

        // ...while a session-less row against a real task is accepted.
        sqlx::query(
            "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
             VALUES ('s1', 't1', NULL, 'stage_entered', '{}', '2026-01-01T00:00:06Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
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

    /// 0005 drops `tasks.parent_task_id` (#83) with `ALTER TABLE ... DROP
    /// COLUMN` rather than the create-copy-drop-rename rebuild 0003 used on
    /// `events`, because `tasks` is the *target* of foreign keys from
    /// `task_runs`, `workflow_state` and `events` — `DROP TABLE tasks` fails
    /// against those rows, and `PRAGMA foreign_keys` can't be turned off
    /// inside the transaction sqlx runs each migration in. This asserts what
    /// that choice has to preserve: the task rows themselves, and the
    /// dependants still pointing at them.
    #[tokio::test]
    async fn dropping_parent_task_id_preserves_tasks_and_their_dependants() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        // Everything up to (not including) 0005, so the fixture below can be
        // written with the column still present.
        for sql in [
            include_str!("../../migrations/0001_init.sql"),
            include_str!("../../migrations/0002_task_run_end_reason.sql"),
            include_str!("../../migrations/0003_stage_transition_events.sql"),
            include_str!("../../migrations/0004_task_worktree_snapshot.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }

        // A delegating pair — the only rows the dropped column ever had a
        // use for — plus the dependants whose foreign keys rule the rebuild
        // out.
        sqlx::raw_sql(
            r#"
INSERT INTO projects (id, name, created_at)
    VALUES ('p1', 'demo', '2026-01-01T00:00:00Z');
INSERT INTO tasks (id, project_id, parent_task_id, workflow_def, title, created_at, updated_at)
    VALUES ('t1', 'p1', NULL, 'chat', 'Parent', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
           ('t2', 'p1', 't1', 'chat', 'Child',  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, status, started_at)
    VALUES ('r1', 't1', 'chatting', 'chat', 'claude', 'sonnet', 'idle', '2026-01-01T00:00:00Z');
INSERT INTO workflow_state (task_id, current_stage, loop_counters, payload, updated_at)
    VALUES ('t1', 'chatting', '{}', '{}', '2026-01-01T00:00:00Z');
"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../../migrations/0005_drop_task_parent_task_id.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        // Both tasks survive, including the one that was a child.
        let titles: Vec<(String,)> = sqlx::query_as("SELECT title FROM tasks ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            titles.into_iter().map(|(t,)| t).collect::<Vec<_>>(),
            vec!["Parent".to_string(), "Child".to_string()]
        );
        assert!(
            sqlx::query("SELECT parent_task_id FROM tasks")
                .fetch_optional(&pool)
                .await
                .is_err(),
            "parent_task_id should no longer exist"
        );

        // The dependants are still attached...
        let (runs,): (i64,) = sqlx::query_as("SELECT count(*) FROM task_runs WHERE task_id = 't1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(runs, 1);
        // ...and `task_runs.task_id` still resolves against `tasks` and
        // rejects an orphan. The constraint is declared on `task_runs`, which
        // this migration never touches — what's under test is that rewriting
        // `tasks` out from under it left the reference intact.
        let orphan = sqlx::query(
            "INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, status, started_at)
             VALUES ('r2', 'no-such-task', 's', 'r', 'claude', 'sonnet', 'idle', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await;
        // The error is matched, not just its existence: the identical column
        // list inserts fine for 'r1' above, so anything *other* than an FK
        // violation here would mean the assertion had stopped testing the FK.
        let err = orphan.expect_err("task_runs.task_id FK should still be enforced");
        assert!(
            err.to_string().contains("FOREIGN KEY"),
            "expected a foreign-key violation, got: {err}"
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
