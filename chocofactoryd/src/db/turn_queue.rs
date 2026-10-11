//! The queue of agent turns waiting for a slot (`turn_queue`).
//!
//! First in, first out by `id`. Every write is made by the engine's turn
//! gate under its slot lock; this module only holds the SQL.
//!
//! "Admissible" is the one rule for which rows count toward a position and
//! may be started: the row's task is `open`. It exists twice, as
//! [`ADMISSIBLE_SQL`] and as `engine::turns::is_admissible`, and a test pins
//! that they agree for every task status.

use chocofactory_core::models::EventType;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool};

/// The SQL half of the admissible predicate, over the alias `t` for `tasks`.
/// A macro so `concat!` can splice it into a static query string.
macro_rules! admissible_sql {
    () => {
        "t.status = 'open'"
    };
}

pub const ADMISSIBLE_SQL: &str = admissible_sql!();

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct QueueRow {
    pub id: i64,
    pub task_id: String,
    pub title: String,
    pub project_id: String,
    /// The project's own cap; `None` when it shares the server's.
    pub project_cap: Option<i64>,
    pub task_status: String,
    pub stage: String,
    pub group_stage: Option<String>,
    pub group_entry: Option<i64>,
    pub resume_session_id: Option<String>,
    pub queued_at: DateTime<Utc>,
    /// [`ADMISSIBLE_SQL`] evaluated by the database.
    pub admissible: bool,
}

/// A row to insert.
#[derive(Debug, Clone, Copy)]
pub struct NewQueueRow<'a> {
    pub task_id: &'a str,
    pub stage: &'a str,
    pub group_stage: Option<&'a str>,
    pub group_entry: Option<i64>,
    pub resume_session_id: Option<&'a str>,
}

/// Every queued row in queue order, with its task's facts. A row whose task
/// is gone cannot exist (foreign key), so the joins are inner.
pub async fn list(pool: &SqlitePool) -> Result<Vec<QueueRow>, sqlx::Error> {
    sqlx::query_as::<_, QueueRow>(concat!(
        "SELECT q.id, q.task_id, t.title, t.project_id, p.max_agents AS project_cap, \
         t.status AS task_status, q.stage, q.group_stage, q.group_entry, \
         q.resume_session_id, q.queued_at, (",
        admissible_sql!(),
        ") AS admissible \
         FROM turn_queue q \
         JOIN tasks t ON t.id = q.task_id \
         JOIN projects p ON p.id = t.project_id \
         ORDER BY q.id"
    ))
    .fetch_all(pool)
    .await
}

/// Inserts a row and returns its id. A second row for the same
/// `(task, stage)` is a UNIQUE violation, which is the point.
pub async fn insert(pool: &SqlitePool, row: NewQueueRow<'_>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO turn_queue (task_id, stage, group_stage, group_entry, resume_session_id, \
         queued_at) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(row.task_id)
    .bind(row.stage)
    .bind(row.group_stage)
    .bind(row.group_entry)
    .bind(row.resume_session_id)
    .bind(Utc::now())
    .fetch_one(pool)
    .await
}

/// Deletes one row; whether it existed.
pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM turn_queue WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes row `id` only while its task still has status `status` — the
/// status the caller judged the row by. One statement, so a task that
/// changed status in between keeps its row. Whether a row was deleted.
pub async fn delete_if_task_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM turn_queue WHERE id = ? \
         AND task_id IN (SELECT id FROM tasks WHERE status = ?)",
    )
    .bind(id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes every row of `task_id` and returns what was deleted
/// `(stage, group_stage)` — one statement.
pub async fn delete_for_task(
    pool: &SqlitePool,
    task_id: &str,
) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("DELETE FROM turn_queue WHERE task_id = ? RETURNING stage, group_stage")
        .bind(task_id)
        .fetch_all(pool)
        .await
}

/// Whether `(task, stage)` has a queue row. For a branch pass the group
/// entry as well: a row left by an earlier entry of the group does not count.
pub async fn exists(
    pool: &SqlitePool,
    task_id: &str,
    stage: &str,
    group_entry: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let found: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM turn_queue WHERE task_id = ? AND stage = ? \
         AND (? IS NULL OR group_entry = ?)",
    )
    .bind(task_id)
    .bind(stage)
    .bind(group_entry)
    .bind(group_entry)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// The payload of a `turn_withdrawn` event.
pub fn withdrawn_payload(stage: &str, group: Option<&str>, reason: &str) -> Value {
    json!({ "stage": stage, "group": group, "reason": reason })
}

/// Best-effort timeline write for a withdrawn row, after its delete.
pub async fn record_withdrawn(
    pool: &SqlitePool,
    task_id: &str,
    stage: &str,
    group: Option<&str>,
    reason: &str,
) {
    if let Err(err) = crate::db::events::append_for_task(
        pool,
        task_id,
        EventType::TurnWithdrawn,
        withdrawn_payload(stage, group, reason),
    )
    .await
    {
        tracing::error!(task_id, stage, %err, "failed to record a turn_withdrawn event");
    }
}
