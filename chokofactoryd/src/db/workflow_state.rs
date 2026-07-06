use chokofactory_core::models::WorkflowState;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, SqlitePool};

const COLUMNS: &str = "task_id, current_stage, loop_counters, stage_history, payload, updated_at";

#[derive(FromRow)]
struct WorkflowStateRow {
    task_id: i64,
    current_stage: String,
    loop_counters: Json<Value>,
    stage_history: Json<Value>,
    payload: Json<Value>,
    updated_at: DateTime<Utc>,
}

impl From<WorkflowStateRow> for WorkflowState {
    fn from(row: WorkflowStateRow) -> Self {
        WorkflowState {
            task_id: row.task_id,
            current_stage: row.current_stage,
            loop_counters: row.loop_counters.0,
            stage_history: row.stage_history.0,
            payload: row.payload.0,
            updated_at: row.updated_at,
        }
    }
}

/// Creates the single workflow_state row for a task, seeding empty loop
/// counters/history/payload (§3).
pub async fn create(
    pool: &SqlitePool,
    task_id: i64,
    current_stage: &str,
) -> Result<WorkflowState, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, WorkflowStateRow>(&format!(
        "INSERT INTO workflow_state (task_id, current_stage, loop_counters, stage_history, payload, updated_at)
         VALUES (?, ?, '{{}}', '[]', '{{}}', ?)
         RETURNING {COLUMNS}"
    ))
    .bind(task_id)
    .bind(current_stage)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, task_id: i64) -> Result<Option<WorkflowState>, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowStateRow>(&format!(
        "SELECT {COLUMNS} FROM workflow_state WHERE task_id = ?"
    ))
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub struct WorkflowStateUpdate {
    pub current_stage: String,
    pub loop_counters: Value,
    pub stage_history: Value,
    pub payload: Value,
}

pub async fn update(
    pool: &SqlitePool,
    task_id: i64,
    update: WorkflowStateUpdate,
) -> Result<Option<WorkflowState>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, WorkflowStateRow>(&format!(
        "UPDATE workflow_state
         SET current_stage = ?, loop_counters = ?, stage_history = ?, payload = ?, updated_at = ?
         WHERE task_id = ?
         RETURNING {COLUMNS}"
    ))
    .bind(update.current_stage)
    .bind(Json(update.loop_counters))
    .bind(Json(update.stage_history))
    .bind(Json(update.payload))
    .bind(now)
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn delete(pool: &SqlitePool, task_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM workflow_state WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects, tasks};
    use serde_json::json;

    async fn seed_task(pool: &SqlitePool) -> i64 {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id,
                parent_task_id: None,
                workflow_def: "coding_task",
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

        let created = create(&pool, task_id, "coding").await.unwrap();
        assert_eq!(created.current_stage, "coding");
        assert_eq!(created.loop_counters, json!({}));
        assert_eq!(created.stage_history, json!([]));

        let updated = update(
            &pool,
            task_id,
            WorkflowStateUpdate {
                current_stage: "internal_review".to_string(),
                loop_counters: json!({"internal_review": 1}),
                stage_history: json!(["coding"]),
                payload: json!({"pr_url": null}),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.current_stage, "internal_review");
        assert_eq!(updated.loop_counters["internal_review"], 1);

        let fetched = get(&pool, task_id).await.unwrap().unwrap();
        assert_eq!(fetched, updated);

        assert!(delete(&pool, task_id).await.unwrap());
        assert!(get(&pool, task_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(get(&pool, 999).await.unwrap().is_none());
    }
}
