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
    let mut conn = pool.acquire().await?;
    update_in(&mut conn, task_id, update).await
}

/// [`update`] on a caller-supplied connection, so it can run inside a
/// caller's transaction (pass `&mut *tx`).
pub async fn update_in(
    conn: &mut sqlx::SqliteConnection,
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
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(Into::into))
}

/// Result of [`settle_with_failures`].
#[derive(Debug)]
pub struct SettledWithFailures {
    pub state: WorkflowState,
    /// true: the task was `open` and is now `stuck` with `reason`.
    /// false: "not marked" — the task was no longer `open`; its status and
    /// stuck_reason are untouched.
    pub marked_stuck: bool,
}

/// Writes the `workflow_state` update and marks the task `stuck` in one
/// transaction, so neither lands without the other. `Ok(None)` means no
/// `workflow_state` row (nothing written). A task no longer `open` still gets
/// the payload committed and comes back with `marked_stuck: false`. Any
/// failure drops the transaction, which rolls both writes back. The `Error`
/// timeline event is the caller's to append after this returns.
pub async fn settle_with_failures(
    pool: &SqlitePool,
    task_id: &str,
    update: WorkflowStateUpdate,
    reason: &str,
) -> Result<Option<SettledWithFailures>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(state) = update_in(&mut tx, task_id, update).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let marked_stuck = crate::db::tasks::mark_stuck_in(&mut tx, task_id, reason).await?;
    tx.commit().await?;
    Ok(Some(SettledWithFailures {
        state,
        marked_stuck,
    }))
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
                base_ref: None,
                base_commit: None,
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

    const FORCE_STUCK_FAILURE: &str = "CREATE TRIGGER forced BEFORE UPDATE OF status ON tasks \
         BEGIN SELECT RAISE(ABORT, 'forced'); END";

    fn settle_update() -> WorkflowStateUpdate {
        WorkflowStateUpdate {
            current_stage: "review".to_string(),
            stage_kind: "agent_turn".to_string(),
            loop_counters: json!({"x": 1}),
            payload: json!({"a": 2}),
            enters_stage: true,
        }
    }

    async fn seeded_with_row(pool: &SqlitePool) -> String {
        let task_id = seed_task(pool).await;
        create(pool, &task_id, "coding", "agent_turn", json!({"a": 1}))
            .await
            .unwrap();
        task_id
    }

    #[tokio::test]
    async fn settle_with_failures_lands_both_writes() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seeded_with_row(&pool).await;

        let s = settle_with_failures(&pool, &task_id, settle_update(), "r")
            .await
            .unwrap()
            .unwrap();
        assert!(s.marked_stuck);
        assert_eq!(s.state.current_stage, "review");
        let row = get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(row.current_stage, "review");
        assert_eq!(row.payload, json!({"a": 2}));
        assert_eq!(row.loop_counters, json!({"x": 1}));
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "stuck");
        assert_eq!(task.stuck_reason.as_deref(), Some("r"));
    }

    #[tokio::test]
    async fn settle_with_failures_rolls_back_when_the_stuck_write_fails() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seeded_with_row(&pool).await;
        let before = get(&pool, &task_id).await.unwrap().unwrap();
        sqlx::query(FORCE_STUCK_FAILURE)
            .execute(&pool)
            .await
            .unwrap();

        let result = settle_with_failures(&pool, &task_id, settle_update(), "r").await;
        assert!(result.is_err());
        assert_eq!(get(&pool, &task_id).await.unwrap().unwrap(), before);
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open");
        assert_eq!(task.stuck_reason, None);
    }

    #[tokio::test]
    async fn settle_with_failures_on_a_task_that_is_not_open_commits_the_payload_only() {
        let pool = connect_in_memory().await.unwrap();
        let cancelled = seeded_with_row(&pool).await;
        tasks::update_status(&pool, &cancelled, "cancelled")
            .await
            .unwrap();
        let stuck = seeded_with_row(&pool).await;
        assert!(tasks::mark_stuck(&pool, &stuck, "first").await.unwrap());

        for (id, status, reason) in [
            (&cancelled, "cancelled", None),
            (&stuck, "stuck", Some("first")),
        ] {
            let s = settle_with_failures(&pool, id, settle_update(), "second")
                .await
                .unwrap()
                .unwrap();
            assert!(!s.marked_stuck);
            assert_eq!(
                get(&pool, id).await.unwrap().unwrap().payload,
                json!({"a": 2})
            );
            let task = tasks::get(&pool, id).await.unwrap().unwrap();
            assert_eq!(task.status, status);
            assert_eq!(task.stuck_reason.as_deref(), reason);
        }
    }

    #[tokio::test]
    async fn settle_with_failures_without_a_row_writes_nothing() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let result = settle_with_failures(&pool, &task_id, settle_update(), "r")
            .await
            .unwrap();
        assert!(result.is_none());
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open");
        assert_eq!(task.stuck_reason, None);
        assert!(get(&pool, &task_id).await.unwrap().is_none());
    }
}
