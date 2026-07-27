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

/// Every event across all of `task_id`'s `task_runs`, oldest first (P1-9).
/// `events` has no `task_id` column of its own — a task can span multiple
/// `task_runs` over its lifetime (idle/resume cycles, §4.1) — so this joins
/// through `task_runs.task_id`. `seq` is only unique per `task_run_id`
/// (`UNIQUE(task_run_id, seq)`), not a total order across runs, so ordering
/// here is `created_at, id` instead — the same tie-break shape already used
/// by `task_runs::get_current_for_stage`'s `ORDER BY started_at DESC, id
/// DESC` for the same "chrono column plus a deterministic tie-break" need.
pub async fn list_for_task(pool: &SqlitePool, task_id: &str) -> Result<Vec<Event>, sqlx::Error> {
    let prefixed_columns = prefix_columns();
    let rows = sqlx::query_as::<_, EventRow>(&format!(
        "SELECT {prefixed_columns} FROM events e
         JOIN task_runs tr ON tr.id = e.task_run_id
         WHERE tr.task_id = ?
         ORDER BY e.created_at, e.id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Same total order as [`list_for_task`], but only events strictly after
/// `cursor` — the `(created_at, id)` of the last event a caller has already
/// seen (P1-9, live WS streaming). `None` means "from the beginning."
pub async fn list_for_task_after(
    pool: &SqlitePool,
    task_id: &str,
    cursor: Option<&(DateTime<Utc>, String)>,
) -> Result<Vec<Event>, sqlx::Error> {
    let prefixed_columns = prefix_columns();
    let mut query = sqlx::QueryBuilder::new(format!(
        "SELECT {prefixed_columns} FROM events e
         JOIN task_runs tr ON tr.id = e.task_run_id
         WHERE tr.task_id = "
    ));
    query.push_bind(task_id);
    if let Some((created_at, id)) = cursor {
        query.push(" AND (e.created_at > ");
        query.push_bind(*created_at);
        query.push(" OR (e.created_at = ");
        query.push_bind(*created_at);
        query.push(" AND e.id > ");
        query.push_bind(id.clone());
        query.push("))");
    }
    query.push(" ORDER BY e.created_at, e.id");

    let rows = query.build_query_as::<EventRow>().fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `COLUMNS`, but each column prefixed with `e.` so it's unambiguous once
/// joined against `task_runs` (which also has an `id` column).
fn prefix_columns() -> String {
    COLUMNS
        .split(", ")
        .map(|col| format!("e.{col}"))
        .collect::<Vec<_>>()
        .join(", ")
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

    async fn seed_task_run_for_task(pool: &SqlitePool, task_id: &str, stage: &str) -> String {
        task_runs::create(
            pool,
            task_runs::NewTaskRun {
                task_id,
                stage,
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
    async fn list_for_task_orders_events_across_multiple_task_runs() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
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

        // Two task_runs under the same task (an idle/resume cycle, §4.1) —
        // each has its own seq starting at 1, so a naive per-run cursor
        // can't tell these apart; only the join on task_id can.
        let run_a = seed_task_run_for_task(&pool, &task_id, "chatting").await;
        let e1 = append(&pool, &run_a, EventType::AssistantMessage, json!({"n": 1}))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let run_b = seed_task_run_for_task(&pool, &task_id, "chatting").await;
        let e2 = append(&pool, &run_b, EventType::AssistantMessage, json!({"n": 2}))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let e3 = append(&pool, &run_a, EventType::AssistantMessage, json!({"n": 3}))
            .await
            .unwrap();

        let all = list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(
            all.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![e1.id.clone(), e2.id.clone(), e3.id.clone()]
        );

        let cursor = (e1.created_at, e1.id.clone());
        let after = list_for_task_after(&pool, &task_id, Some(&cursor))
            .await
            .unwrap();
        assert_eq!(
            after.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![e2.id, e3.id]
        );

        let none_yet = list_for_task_after(&pool, &task_id, None).await.unwrap();
        assert_eq!(none_yet.len(), 3);
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
