use chokofactory_core::models::{Event, EventType};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const COLUMNS: &str = "id, task_run_id, seq, event_type, payload, created_at";

#[derive(FromRow)]
struct EventRow {
    id: String,
    task_run_id: String,
    seq: i64,
    event_type: String,
    payload: Json<Value>,
    created_at: DateTime<Utc>,
}

impl From<EventRow> for Event {
    fn from(row: EventRow) -> Self {
        Event {
            id: row.id,
            task_run_id: row.task_run_id,
            seq: row.seq,
            event_type: row
                .event_type
                .parse()
                .expect("events.event_type holds a value written by this module"),
            payload: row.payload.0,
            created_at: row.created_at,
        }
    }
}

/// Appends a normalized event, assigning it the next `seq` for its
/// `task_run_id` (§4.2). The log is append-only: there is no update.
pub async fn append(
    pool: &SqlitePool,
    task_run_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<Event, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, EventRow>(&format!(
        "INSERT INTO events (id, task_run_id, seq, event_type, payload, created_at)
         SELECT ?, ?, COALESCE(MAX(seq), 0) + 1, ?, ?, ?
         FROM events WHERE task_run_id = ?
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(task_run_id)
    .bind(event_type.to_string())
    .bind(Json(payload))
    .bind(now)
    .bind(task_run_id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Event>, sqlx::Error> {
    let row = sqlx::query_as::<_, EventRow>(&format!("SELECT {COLUMNS} FROM events WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

pub async fn list_for_task_run(
    pool: &SqlitePool,
    task_run_id: &str,
) -> Result<Vec<Event>, sqlx::Error> {
    let rows = sqlx::query_as::<_, EventRow>(&format!(
        "SELECT {COLUMNS} FROM events WHERE task_run_id = ? ORDER BY seq"
    ))
    .bind(task_run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Prunes events older than `cutoff`, returning the number of rows removed.
/// Backs the 1-year retention job (§4.4); leaves `tasks`/`task_runs` alone.
pub async fn delete_older_than(
    pool: &SqlitePool,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM events WHERE created_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects, task_runs, tasks};
    use chrono::Duration;
    use serde_json::json;

    async fn seed_task_run(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
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
        .id;
        task_runs::create(
            pool,
            task_runs::NewTaskRun {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn append_assigns_increasing_seq() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;

        let e1 = append(
            &pool,
            &task_run_id,
            EventType::AssistantMessage,
            json!({"text": "hi"}),
        )
        .await
        .unwrap();
        let e2 = append(
            &pool,
            &task_run_id,
            EventType::ToolCall,
            json!({"tool": "bash"}),
        )
        .await
        .unwrap();

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert!(!e1.id.is_empty());

        let fetched = get(&pool, &e1.id).await.unwrap().unwrap();
        assert_eq!(fetched.event_type, EventType::AssistantMessage);

        let all = list_for_task_run(&pool, &task_run_id).await.unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[tokio::test]
    async fn seq_is_scoped_per_task_run() {
        let pool = connect_in_memory().await.unwrap();
        let run_a = seed_task_run(&pool).await;
        let run_b = seed_task_run(&pool).await;

        let a1 = append(&pool, &run_a, EventType::Thinking, json!({}))
            .await
            .unwrap();
        let b1 = append(&pool, &run_b, EventType::Thinking, json!({}))
            .await
            .unwrap();

        assert_eq!(a1.seq, 1);
        assert_eq!(b1.seq, 1);
    }

    #[tokio::test]
    async fn delete_older_than_prunes_only_stale_events() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        append(&pool, &task_run_id, EventType::Error, json!({}))
            .await
            .unwrap();

        let cutoff_in_future = Utc::now() + Duration::seconds(1);
        let removed = delete_older_than(&pool, cutoff_in_future).await.unwrap();
        assert_eq!(removed, 1);
        assert!(
            list_for_task_run(&pool, &task_run_id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
