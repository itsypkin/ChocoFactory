use chokofactory_core::models::{TaskRun, TaskRunStatus};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const COLUMNS: &str =
    "id, task_id, stage, role, cli_adapter, model, session_id, status, started_at, ended_at";

#[derive(FromRow)]
struct TaskRunRow {
    id: String,
    task_id: String,
    stage: String,
    role: String,
    cli_adapter: String,
    model: String,
    session_id: Option<String>,
    status: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

impl From<TaskRunRow> for TaskRun {
    fn from(row: TaskRunRow) -> Self {
        TaskRun {
            id: row.id,
            task_id: row.task_id,
            stage: row.stage,
            role: row.role,
            cli_adapter: row.cli_adapter,
            model: row.model,
            session_id: row.session_id,
            status: row
                .status
                .parse()
                .expect("task_runs.status holds a value written by this module"),
            started_at: row.started_at,
            ended_at: row.ended_at,
        }
    }
}

pub struct NewTaskRun<'a> {
    pub task_id: &'a str,
    pub stage: &'a str,
    pub role: &'a str,
    pub cli_adapter: &'a str,
    pub model: &'a str,
}

pub async fn create(pool: &SqlitePool, new: NewTaskRun<'_>) -> Result<TaskRun, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRunRow>(&format!(
        "INSERT INTO task_runs (id, task_id, stage, role, cli_adapter, model, status, started_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(new.task_id)
    .bind(new.stage)
    .bind(new.role)
    .bind(new.cli_adapter)
    .bind(new.model)
    .bind(TaskRunStatus::Active.to_string())
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<TaskRun>, sqlx::Error> {
    let row =
        sqlx::query_as::<_, TaskRunRow>(&format!("SELECT {COLUMNS} FROM task_runs WHERE id = ?"))
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(Into::into))
}

pub async fn list_for_task(pool: &SqlitePool, task_id: &str) -> Result<Vec<TaskRun>, sqlx::Error> {
    let rows = sqlx::query_as::<_, TaskRunRow>(&format!(
        "SELECT {COLUMNS} FROM task_runs WHERE task_id = ? ORDER BY id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Persists the CLI's `session_id` for later resume (§4.1).
pub async fn set_session_id(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
) -> Result<Option<TaskRun>, sqlx::Error> {
    let row = sqlx::query_as::<_, TaskRunRow>(&format!(
        "UPDATE task_runs SET session_id = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(session_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn update_status(
    pool: &SqlitePool,
    id: &str,
    status: TaskRunStatus,
    ended_at: Option<DateTime<Utc>>,
) -> Result<Option<TaskRun>, sqlx::Error> {
    let row = sqlx::query_as::<_, TaskRunRow>(&format!(
        "UPDATE task_runs SET status = ?, ended_at = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(status.to_string())
    .bind(ended_at)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Daemon-restart recovery (§4.3): any run left `active` in the DB when
/// the daemon starts is dead — its process is gone — so it's flipped to
/// `idle` using its already-persisted `session_id`, ready to `resume` on
/// the next message. Call once at startup before any `SessionManager` use.
pub async fn recover_stale_active_runs(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("UPDATE task_runs SET status = ? WHERE status = ?")
        .bind(TaskRunStatus::Idle.to_string())
        .bind(TaskRunStatus::Active.to_string())
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM task_runs WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects, tasks};
    use serde_json::json;

    async fn seed_task(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: "chat",
                title: "T",
                config: json!({}),
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let created = create(
            &pool,
            NewTaskRun {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        assert_eq!(created.status, TaskRunStatus::Active);
        assert!(created.session_id.is_none());
        assert!(!created.id.is_empty());

        let with_session = set_session_id(&pool, &created.id, "sess-123")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with_session.session_id.as_deref(), Some("sess-123"));

        let idle = update_status(&pool, &created.id, TaskRunStatus::Idle, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idle.status, TaskRunStatus::Idle);

        let now = Utc::now();
        let exited = update_status(&pool, &created.id, TaskRunStatus::Exited, Some(now))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exited.status, TaskRunStatus::Exited);
        assert_eq!(exited.ended_at, Some(now));

        let runs = list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(runs.len(), 1);

        assert!(delete(&pool, &created.id).await.unwrap());
        assert!(get(&pool, &created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn recover_stale_active_runs_flips_active_to_idle_and_leaves_others_alone() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;
        let new_run = || NewTaskRun {
            task_id: &task_id,
            stage: "chatting",
            role: "chat",
            cli_adapter: "claude",
            model: "sonnet",
        };

        let active = create(&pool, new_run()).await.unwrap();
        let already_idle = create(&pool, new_run()).await.unwrap();
        update_status(&pool, &already_idle.id, TaskRunStatus::Idle, None)
            .await
            .unwrap();
        let exited = create(&pool, new_run()).await.unwrap();
        update_status(&pool, &exited.id, TaskRunStatus::Exited, Some(Utc::now()))
            .await
            .unwrap();

        let recovered = recover_stale_active_runs(&pool).await.unwrap();
        assert_eq!(recovered, 1);

        assert_eq!(
            get(&pool, &active.id).await.unwrap().unwrap().status,
            TaskRunStatus::Idle
        );
        assert_eq!(
            get(&pool, &already_idle.id).await.unwrap().unwrap().status,
            TaskRunStatus::Idle
        );
        assert_eq!(
            get(&pool, &exited.id).await.unwrap().unwrap().status,
            TaskRunStatus::Exited
        );
    }
}
