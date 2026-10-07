use chocofactory_core::models::WorkflowState;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, SqlitePool};

// A macro rather than a const so `concat!` can build each query as the
// `&'static str` that sqlx 0.9 accepts without `AssertSqlSafe`.
macro_rules! columns {
    () => {
        "task_id, current_stage, stage_kind, loop_counters, payload, updated_at, stage_entered_at"
    };
}

#[derive(FromRow)]
struct WorkflowStateRow {
    task_id: String,
    current_stage: String,
    stage_kind: Option<String>,
    loop_counters: Json<Value>,
    payload: Json<Value>,
    updated_at: DateTime<Utc>,
    stage_entered_at: Option<DateTime<Utc>>,
}

impl From<WorkflowStateRow> for WorkflowState {
    fn from(row: WorkflowStateRow) -> Self {
        WorkflowState {
            task_id: row.task_id,
            current_stage: row.current_stage,
            stage_kind: row.stage_kind,
            loop_counters: row.loop_counters.0,
            payload: row.payload.0,
            updated_at: row.updated_at,
            stage_entered_at: row.stage_entered_at,
        }
    }
}

/// Creates the single workflow_state row for a task, seeding empty loop
/// counters (§3) and the given `payload`. The stage trail lives in the
/// events timeline instead, as `stage_entered` entries (X-3).
///
/// `payload` lets `start_task` seed `payload.task` (P2-7a: a task's title
/// and initial input, reachable from a `prompt_file` as `{{ task.* }}`) in
/// the same statement the row is created with, rather than a separate
/// create-then-update — there is no window where the row exists without it.
pub async fn create(
    pool: &SqlitePool,
    task_id: &str,
    current_stage: &str,
    stage_kind: &str,
    payload: Value,
) -> Result<WorkflowState, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, WorkflowStateRow>(concat!(
        "INSERT INTO workflow_state (task_id, current_stage, stage_kind, loop_counters, payload, updated_at, stage_entered_at)
         VALUES (?, ?, ?, '{}', ?, ?, ?)
         RETURNING ", columns!()
    ))
    .bind(task_id)
    .bind(current_stage)
    .bind(stage_kind)
    .bind(Json(payload))
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, task_id: &str) -> Result<Option<WorkflowState>, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowStateRow>(concat!(
        "SELECT ",
        columns!(),
        " FROM workflow_state WHERE task_id = ?"
    ))
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub struct WorkflowStateUpdate {
    pub current_stage: String,
    /// Kind of `current_stage`, written in the same UPDATE.
    pub stage_kind: String,
    pub loop_counters: Value,
    pub payload: Value,
    /// `true` stamps `stage_entered_at` with the same instant as
    /// `updated_at`, in the same UPDATE; `false` leaves it as it is.
    pub enters_stage: bool,
}

pub async fn update(
    pool: &SqlitePool,
    task_id: &str,
    update: WorkflowStateUpdate,
) -> Result<Option<WorkflowState>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, WorkflowStateRow>(concat!(
        "UPDATE workflow_state
         SET current_stage = ?, stage_kind = ?, loop_counters = ?, payload = ?, updated_at = ?,
             stage_entered_at = CASE WHEN ? THEN ? ELSE stage_entered_at END
         WHERE task_id = ?
         RETURNING ",
        columns!()
    ))
    .bind(update.current_stage)
    .bind(update.stage_kind)
    .bind(Json(update.loop_counters))
    .bind(Json(update.payload))
    .bind(now)
    .bind(update.enters_stage)
    .bind(now)
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Records the kind of the task's current stage without touching anything
/// else — notably not `updated_at` or `stage_entered_at`, since the startup
/// sweep derives a missing poll window from `updated_at`. Only for filling in
/// a row that predates the column or went stale; a transition writes the kind
/// in its own UPDATE. `false` means no row.
pub async fn set_stage_kind(
    pool: &SqlitePool,
    task_id: &str,
    kind: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE workflow_state SET stage_kind = ? WHERE task_id = ?")
        .bind(kind)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete(pool: &SqlitePool, task_id: &str) -> Result<bool, sqlx::Error> {
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

    async fn seed_task(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "coding_task",
                title: "T",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
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

        let created = create(&pool, &task_id, "coding", "agent_turn", json!({}))
            .await
            .unwrap();
        assert_eq!(created.current_stage, "coding");
        assert_eq!(created.loop_counters, json!({}));

        let updated = update(
            &pool,
            &task_id,
            WorkflowStateUpdate {
                current_stage: "internal_review".to_string(),
                stage_kind: "agent_turn".to_string(),
                loop_counters: json!({"internal_review": 1}),
                payload: json!({"pr_url": null}),
                enters_stage: true,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.current_stage, "internal_review");
        assert_eq!(updated.loop_counters["internal_review"], 1);

        let fetched = get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(fetched, updated);

        assert!(delete(&pool, &task_id).await.unwrap());
        assert!(get(&pool, &task_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stage_entered_at_is_stamped_only_when_entering() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let created = create(&pool, &task_id, "coding", "agent_turn", json!({}))
            .await
            .unwrap();
        assert_eq!(created.stage_entered_at, Some(created.updated_at));

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let entered = update(
            &pool,
            &task_id,
            WorkflowStateUpdate {
                current_stage: "review".into(),
                stage_kind: "agent_turn".into(),
                loop_counters: json!({}),
                payload: json!({}),
                enters_stage: true,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(entered.stage_entered_at, Some(entered.updated_at));
        assert!(entered.stage_entered_at > created.stage_entered_at);

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let kept = update(
            &pool,
            &task_id,
            WorkflowStateUpdate {
                current_stage: "review".into(),
                stage_kind: "agent_turn".into(),
                loop_counters: json!({}),
                payload: json!({"changed": true}),
                enters_stage: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(kept.stage_entered_at, entered.stage_entered_at);
        assert!(kept.updated_at > entered.updated_at);
        assert_eq!(kept.payload, json!({"changed": true}));
    }

    #[tokio::test]
    async fn set_stage_kind_touches_only_the_kind() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;
        let created = create(&pool, &task_id, "gate", "poll", json!({}))
            .await
            .unwrap();
        assert_eq!(created.stage_kind.as_deref(), Some("poll"));

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(set_stage_kind(&pool, &task_id, "human_gate").await.unwrap());
        let after = get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(after.stage_kind.as_deref(), Some("human_gate"));
        assert_eq!(after.updated_at, created.updated_at);
        assert_eq!(after.stage_entered_at, created.stage_entered_at);

        assert!(!set_stage_kind(&pool, "missing", "poll").await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(get(&pool, "does-not-exist").await.unwrap().is_none());
    }

    /// `create` binds `payload` into the same `INSERT` rather than seeding
    /// `{}` and relying on a caller to `update` it in afterwards (P2-7a) —
    /// this is the row `start_task` needs, with `payload.task` already set.
    #[tokio::test]
    async fn create_persists_a_seeded_payload() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let created = create(
            &pool,
            &task_id,
            "coding",
            "agent_turn",
            json!({"task": {"input": "fix the flaky test", "title": "T"}}),
        )
        .await
        .unwrap();
        assert_eq!(created.payload["task"]["input"], "fix the flaky test");
        assert_eq!(created.payload["task"]["title"], "T");

        let fetched = get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(fetched.payload, created.payload);
    }
}
