use chokofactory_core::models::Task;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, QueryBuilder, SqlitePool};

const COLUMNS: &str =
    "id, project_id, parent_task_id, workflow_def, title, status, config, created_at, updated_at";

#[derive(FromRow)]
struct TaskRow {
    id: i64,
    project_id: i64,
    parent_task_id: Option<i64>,
    workflow_def: String,
    title: String,
    status: String,
    config: Json<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Task {
            id: row.id,
            project_id: row.project_id,
            parent_task_id: row.parent_task_id,
            workflow_def: row.workflow_def,
            title: row.title,
            status: row.status,
            config: row.config.0,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

pub struct NewTask<'a> {
    pub project_id: i64,
    pub parent_task_id: Option<i64>,
    pub workflow_def: &'a str,
    pub title: &'a str,
    pub config: Value,
}

pub async fn create(pool: &SqlitePool, new: NewTask<'_>) -> Result<Task, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "INSERT INTO tasks (project_id, parent_task_id, workflow_def, title, status, config, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'open', ?, ?, ?)
         RETURNING {COLUMNS}"
    ))
    .bind(new.project_id)
    .bind(new.parent_task_id)
    .bind(new.workflow_def)
    .bind(new.title)
    .bind(Json(new.config))
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Task>, sqlx::Error> {
    let row = sqlx::query_as::<_, TaskRow>(&format!("SELECT {COLUMNS} FROM tasks WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

/// Lists tasks, optionally filtered by project and/or status (§6.2's
/// `choco task list [--project <p>] [--status <s>]`).
pub async fn list(
    pool: &SqlitePool,
    project_id: Option<i64>,
    status: Option<&str>,
) -> Result<Vec<Task>, sqlx::Error> {
    let mut qb = QueryBuilder::new(format!("SELECT {COLUMNS} FROM tasks"));
    let mut has_where = false;
    if let Some(project_id) = project_id {
        qb.push(" WHERE project_id = ").push_bind(project_id);
        has_where = true;
    }
    if let Some(status) = status {
        qb.push(if has_where {
            " AND status = "
        } else {
            " WHERE status = "
        });
        qb.push_bind(status.to_string());
    }
    qb.push(" ORDER BY id");
    let rows = qb.build_query_as::<TaskRow>().fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn update_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
) -> Result<Option<Task>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "UPDATE tasks SET status = ?, updated_at = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(status)
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn update_config(
    pool: &SqlitePool,
    id: i64,
    config: Value,
) -> Result<Option<Task>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "UPDATE tasks SET config = ?, updated_at = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(Json(config))
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM tasks WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects};
    use serde_json::json;

    async fn seed_project(pool: &SqlitePool) -> i64 {
        projects::create(pool, "demo").await.unwrap().id
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = seed_project(&pool).await;

        let created = create(
            &pool,
            NewTask {
                project_id,
                parent_task_id: None,
                workflow_def: "chat",
                title: "Investigate flaky test",
                config: json!({"model": "sonnet"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(created.status, "open");
        assert_eq!(created.config["model"], "sonnet");

        let fetched = get(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(fetched, created);

        let updated = update_status(&pool, created.id, "closed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "closed");
        assert!(updated.updated_at >= created.updated_at);

        let reconfigured = update_config(&pool, created.id, json!({"model": "opus"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reconfigured.config["model"], "opus");

        assert!(delete(&pool, created.id).await.unwrap());
        assert!(get(&pool, created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_filters_by_project_and_status() {
        let pool = connect_in_memory().await.unwrap();
        let project_a = seed_project(&pool).await;
        let project_b = seed_project(&pool).await;

        let t1 = create(
            &pool,
            NewTask {
                project_id: project_a,
                parent_task_id: None,
                workflow_def: "chat",
                title: "A1",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        create(
            &pool,
            NewTask {
                project_id: project_b,
                parent_task_id: None,
                workflow_def: "chat",
                title: "B1",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        update_status(&pool, t1.id, "closed").await.unwrap();

        let in_a = list(&pool, Some(project_a), None).await.unwrap();
        assert_eq!(in_a.len(), 1);
        assert_eq!(in_a[0].title, "A1");

        let closed = list(&pool, None, Some("closed")).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, t1.id);

        let all = list(&pool, None, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn parent_task_id_round_trips() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = seed_project(&pool).await;
        let parent = create(
            &pool,
            NewTask {
                project_id,
                parent_task_id: None,
                workflow_def: "coding_task",
                title: "Parent",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        let child = create(
            &pool,
            NewTask {
                project_id,
                parent_task_id: Some(parent.id),
                workflow_def: "chat",
                title: "Child",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        assert_eq!(child.parent_task_id, Some(parent.id));
    }
}
