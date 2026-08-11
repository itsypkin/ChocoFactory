use chokofactory_core::models::{Event, EventType};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const COLUMNS: &str = "id, task_id, task_run_id, event_type, payload, created_at";

#[derive(FromRow)]
struct EventRow {
    id: String,
    task_id: String,
    task_run_id: Option<String>,
    event_type: String,
    payload: Json<Value>,
    created_at: DateTime<Utc>,
}

impl From<EventRow> for Event {
    fn from(row: EventRow) -> Self {
        Event {
            id: row.id,
            task_id: row.task_id,
            task_run_id: row.task_run_id,
            event_type: row
                .event_type
                .parse()
                .expect("events.event_type holds a value written by this module"),
            payload: row.payload.0,
            created_at: row.created_at,
        }
    }
}

/// Appends an event produced by an agent session (§4.2). The log is
/// append-only: there is no update.
///
/// `task_id` is denormalized onto every row so the task timeline needs no
/// join (X-3), and is read here from the run itself rather than taken as a
/// parameter — callers only ever hold a `task_run_id`, and deriving it
/// inside the INSERT keeps the two consistent by construction. A
/// `task_run_id` that doesn't exist therefore selects no row and surfaces
/// as `RowNotFound` rather than as a foreign-key violation.
pub async fn append(
    pool: &SqlitePool,
    task_run_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<Event, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, EventRow>(&format!(
        "INSERT INTO events (id, task_id, task_run_id, event_type, payload, created_at)
         SELECT ?, tr.task_id, tr.id, ?, ?, ?
         FROM task_runs tr WHERE tr.id = ?
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(event_type.to_string())
    .bind(Json(payload))
    .bind(now)
    .bind(task_run_id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

/// Appends an entry that belongs to the *task* rather than to any agent
/// session, so `task_run_id` is NULL (X-3): stage transitions happen in
/// stages that never open a session (`human_gate`, `terminal`) and before
/// one exists (a task's entry stage), and a `shell` stage (P2-1) has no
/// `task_run` at all. Unlike [`append`] there is no run to derive `task_id`
/// from, so the caller supplies it directly — an unknown `task_id` is a
/// foreign-key violation rather than `RowNotFound`.
pub async fn append_for_task(
    pool: &SqlitePool,
    task_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<Event, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, EventRow>(&format!(
        "INSERT INTO events (id, task_id, task_run_id, event_type, payload, created_at)
         VALUES (?, ?, NULL, ?, ?, ?)
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(task_id)
    .bind(event_type.to_string())
    .bind(Json(payload))
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

/// Appends a stage-transition entry to a task's timeline (X-3).
///
/// `entered_via` is the transition outcome that selected this stage —
/// `None` for the entry stage — and is stored under the payload key
/// `outcome`.
pub async fn append_stage_transition(
    pool: &SqlitePool,
    task_id: &str,
    stage: &str,
    entered_via: Option<&str>,
) -> Result<Event, sqlx::Error> {
    append_for_task(
        pool,
        task_id,
        EventType::StageEntered,
        json!({ "stage": stage, "outcome": entered_via }),
    )
    .await
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
        "SELECT {COLUMNS} FROM events WHERE task_run_id = ? ORDER BY created_at, id"
    ))
    .bind(task_run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Everything a session's agent actually said, oldest first, joined with
/// newlines — the text an `agent_turn`'s `capture:` is taken from (#45).
///
/// The engine keeps no copy of a turn's reply: `drain_session` appends each
/// `AssistantMessage` here and drops it, and the completion watcher only ever
/// sees `task_runs` rows. So the reply is read back from the timeline.
///
/// Filtering by `event_type` in SQL rather than fetching the run's whole
/// event list and filtering in Rust is deliberate — that list also holds tool
/// results, which can be far larger than the reply and would be loaded only
/// to be discarded.
///
/// Scoped to one run, which on this path is one turn: a stage that captures
/// has a non-empty `on:` map, and `send_message` only accepts stages whose
/// `on:` is empty, so no second turn can be added to this run. A future
/// change that lets a capturing stage take more than one turn would need to
/// bound this further (by the last `human_message`, say).
///
/// Blocks are joined with a newline: the adapter emits one event per text
/// block of a single reply (`normalize_assistant`), so a reply the agent
/// wrote as one JSON document can arrive as several rows that have to be put
/// back together before anything can parse it.
///
/// Decoding through `Json<Value>` rather than parsing the column by hand
/// makes a payload that isn't valid JSON a `sqlx` decode error the caller has
/// to handle, instead of a block quietly dropped from the middle of a reply —
/// which would hand the capture a truncated document that might still parse.
///
/// Reading this after the run reports `idle` is safe by construction:
/// `drain_session` appends every event before it touches the run's status, so
/// a completed run's reply is whole here. The one gap is that those appends
/// are best-effort — a transient DB failure there is logged and dropped, and
/// the block it lost is simply not part of the text this returns. That
/// predates capture; it matters more now that a `capture: json` turn parses
/// the result, where the worst case is a truncated document that still
/// parses into the wrong verdict.
pub async fn assistant_text_for_run(
    pool: &SqlitePool,
    task_run_id: &str,
) -> Result<String, sqlx::Error> {
    let rows: Vec<(Json<Value>,)> = sqlx::query_as(
        "SELECT payload FROM events
         WHERE task_run_id = ? AND event_type = ?
         ORDER BY created_at, id",
    )
    .bind(task_run_id)
    .bind(EventType::AssistantMessage.to_string())
    .fetch_all(pool)
    .await?;

    let mut blocks = Vec::with_capacity(rows.len());
    for (payload,) in &rows {
        match payload.0.get("text").and_then(Value::as_str) {
            Some(text) => blocks.push(text),
            // Every `assistant_message` this daemon writes carries a `text`
            // string (`AgentEvent::payload`), so this is an invariant
            // violation rather than a shape to tolerate. It can't be a hard
            // error without a richer error type than `sqlx::Error`, but it is
            // loud rather than silent — a dropped block would otherwise
            // silently corrupt the capture.
            None => tracing::error!(
                task_run_id,
                "an assistant_message event has no 'text' field; \
                 it is missing from the text of this run's reply"
            ),
        }
    }
    Ok(blocks.join("\n"))
}

/// A task's whole timeline, oldest first (P1-9) — every session's events
/// interleaved with the task's own `stage_entered` entries.
///
/// Filters `events.task_id` directly. This used to join through
/// `task_runs.task_id`, which is no longer merely slower but *wrong*: a
/// stage transition has no `task_run_id` (X-3) and would be dropped by the
/// join. Ordering is `created_at, id` — the same "chrono column plus a
/// deterministic tie-break" shape already used by
/// `task_runs::get_current_for_stage`'s `ORDER BY started_at DESC, id DESC`.
pub async fn list_for_task(pool: &SqlitePool, task_id: &str) -> Result<Vec<Event>, sqlx::Error> {
    let rows = sqlx::query_as::<_, EventRow>(&format!(
        "SELECT {COLUMNS} FROM events
         WHERE task_id = ?
         ORDER BY created_at, id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Just the `stage_entered` slice of [`list_for_task`], same order — the
/// task's stage trail.
///
/// This is what `workflow_state.stage_history` used to hold before X-3, and
/// it exists as its own query so `GET /tasks/:id` can keep serving the trail
/// in one round trip. Filtering the paginated events endpoint client-side
/// would not do: on a task with a real coding transcript the tool events
/// dominate, so the stage entries fall past the first page and the trail
/// would silently render truncated.
///
/// Served by `idx_events_task_id_created_at`; unbounded in principle, but
/// `stage_history` grew the same way and this version is at least aged out
/// by retention.
pub async fn list_stage_trail(pool: &SqlitePool, task_id: &str) -> Result<Vec<Event>, sqlx::Error> {
    let rows = sqlx::query_as::<_, EventRow>(&format!(
        "SELECT {COLUMNS} FROM events
         WHERE task_id = ? AND event_type = ?
         ORDER BY created_at, id"
    ))
    .bind(task_id)
    .bind(EventType::StageEntered.to_string())
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
    let mut query =
        sqlx::QueryBuilder::new(format!("SELECT {COLUMNS} FROM events WHERE task_id = "));
    query.push_bind(task_id);
    if let Some((created_at, id)) = cursor {
        query.push(" AND (created_at > ");
        query.push_bind(*created_at);
        query.push(" OR (created_at = ");
        query.push_bind(*created_at);
        query.push(" AND id > ");
        query.push_bind(id.clone());
        query.push("))");
    }
    query.push(" ORDER BY created_at, id");

    let rows = query.build_query_as::<EventRow>().fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Same shape as [`list_for_task_after`], but capped to at most `limit`
/// rows (P1-9: paginated `GET /tasks/:id/events`) — unlike the live-WS
/// caller of `list_for_task_after`, a REST page must never load a task's
/// entire history in one response.
pub async fn list_for_task_page(
    pool: &SqlitePool,
    task_id: &str,
    cursor: Option<&(DateTime<Utc>, String)>,
    limit: i64,
) -> Result<Vec<Event>, sqlx::Error> {
    let mut query =
        sqlx::QueryBuilder::new(format!("SELECT {COLUMNS} FROM events WHERE task_id = "));
    query.push_bind(task_id);
    if let Some((created_at, id)) = cursor {
        query.push(" AND (created_at > ");
        query.push_bind(*created_at);
        query.push(" OR (created_at = ");
        query.push_bind(*created_at);
        query.push(" AND id > ");
        query.push_bind(id.clone());
        query.push("))");
    }
    query.push(" ORDER BY created_at, id LIMIT ");
    query.push_bind(limit);

    let rows = query.build_query_as::<EventRow>().fetch_all(pool).await?;
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
    async fn append_derives_the_owning_task_from_the_run() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let owning_task = task_runs::get(&pool, &task_run_id)
            .await
            .unwrap()
            .unwrap()
            .task_id;

        let e1 = append(
            &pool,
            &task_run_id,
            EventType::AssistantMessage,
            json!({"text": "hi"}),
        )
        .await
        .unwrap();

        assert!(!e1.id.is_empty());
        assert_eq!(e1.task_run_id.as_deref(), Some(task_run_id.as_str()));
        // Callers only pass a run; the task is resolved inside the INSERT.
        assert_eq!(e1.task_id, owning_task);

        let fetched = get(&pool, &e1.id).await.unwrap().unwrap();
        assert_eq!(fetched, e1);
        assert_eq!(fetched.event_type, EventType::AssistantMessage);
    }

    #[tokio::test]
    async fn append_against_an_unknown_run_is_an_error_not_an_orphan_row() {
        let pool = connect_in_memory().await.unwrap();

        // The task_id is derived by selecting the run, so a missing run
        // selects nothing and inserts nothing (rather than writing a row
        // with a dangling reference).
        let err = append(&pool, "no-such-run", EventType::Error, json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, sqlx::Error::RowNotFound), "got {err:?}");
    }

    #[tokio::test]
    async fn stage_transitions_are_task_scoped_and_carry_stage_and_outcome() {
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

        // An entry stage: nothing transitioned into it, so no outcome.
        let entry = append_stage_transition(&pool, &task_id, "coding", None)
            .await
            .unwrap();
        assert_eq!(entry.event_type, EventType::StageEntered);
        assert_eq!(entry.task_id, task_id);
        // The point of the change: no session is involved, and none needed
        // to exist for this to be recorded.
        assert_eq!(entry.task_run_id, None);
        assert_eq!(entry.payload["stage"], "coding");
        assert_eq!(entry.payload["outcome"], Value::Null);

        let next = append_stage_transition(&pool, &task_id, "review", Some("approved"))
            .await
            .unwrap();
        assert_eq!(next.payload["stage"], "review");
        assert_eq!(next.payload["outcome"], "approved");

        // Reachable from the task timeline without any task_run existing.
        let timeline = list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(
            timeline.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![entry.id, next.id]
        );
    }

    #[tokio::test]
    async fn stage_transitions_interleave_with_session_events_in_one_timeline() {
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

        let entered = append_stage_transition(&pool, &task_id, "chatting", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let run = seed_task_run_for_task(&pool, &task_id, "chatting").await;
        let said = append(&pool, &run, EventType::AssistantMessage, json!({"n": 1}))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let advanced = append_stage_transition(&pool, &task_id, "done", Some("finished"))
            .await
            .unwrap();

        // Both kinds sort into one `(created_at, id)` order, and the
        // session-scoped row still resolves to the same task.
        let timeline = list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(
            timeline.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![entered.id.clone(), said.id.clone(), advanced.id.clone()]
        );
        assert_eq!(said.task_id, task_id);

        // The cursor-based readers behave identically for both kinds.
        let cursor = (entered.created_at, entered.id.clone());
        let after = list_for_task_after(&pool, &task_id, Some(&cursor))
            .await
            .unwrap();
        assert_eq!(
            after.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![said.id, advanced.id.clone()]
        );

        // A session's own slice excludes the task-level entries.
        let run_only = list_for_task_run(&pool, &run).await.unwrap();
        assert_eq!(run_only.len(), 1);
        assert_eq!(run_only[0].event_type, EventType::AssistantMessage);

        // ...and the trail is the mirror image: the stage entries only, in
        // the same order, with the conversation filtered out. This is what
        // `GET /tasks/:id` serves as `stage_trail`, so a conversation event
        // leaking in would show up as a phantom hop in `choco task status`.
        let trail = list_stage_trail(&pool, &task_id).await.unwrap();
        assert_eq!(
            trail
                .iter()
                .map(|e| (e.id.clone(), e.payload["stage"].clone()))
                .collect::<Vec<_>>(),
            vec![
                (entered.id.clone(), json!("chatting")),
                (advanced.id.clone(), json!("done")),
            ]
        );
    }

    /// The trail is scoped to one task. A second task's transitions sharing
    /// the same `events` table must not bleed into it — that would render
    /// as another task's stages in this one's `choco task status`.
    #[tokio::test]
    async fn the_stage_trail_of_one_task_excludes_another_tasks_transitions() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let new_task = |title: &'static str| tasks::NewTask {
            project_id: &project_id,
            parent_task_id: None,
            workflow_def: "chat",
            title,
            config: json!({}),
        };
        let first = tasks::create(&pool, new_task("A")).await.unwrap().id;
        let second = tasks::create(&pool, new_task("B")).await.unwrap().id;

        append_stage_transition(&pool, &first, "coding", None)
            .await
            .unwrap();
        append_stage_transition(&pool, &second, "triage", None)
            .await
            .unwrap();
        append_stage_transition(&pool, &first, "review", Some("approved"))
            .await
            .unwrap();

        let trail = list_stage_trail(&pool, &first).await.unwrap();
        assert_eq!(
            trail
                .iter()
                .map(|e| e.payload["stage"].clone())
                .collect::<Vec<_>>(),
            vec![json!("coding"), json!("review")]
        );
        assert!(trail.iter().all(|e| e.task_id == first));
    }

    /// A task with no transitions recorded — every task that ran before X-3
    /// — gets an empty trail rather than an error or someone else's rows.
    #[tokio::test]
    async fn the_stage_trail_of_a_task_with_no_transitions_is_empty() {
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

        // A session event exists, so "empty" is the filter working, not the
        // task simply having no events at all.
        let run = seed_task_run_for_task(&pool, &task_id, "chatting").await;
        append(&pool, &run, EventType::AssistantMessage, json!({"n": 1}))
            .await
            .unwrap();

        assert!(list_stage_trail(&pool, &task_id).await.unwrap().is_empty());
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

        // Two task_runs under the same task (an idle/resume cycle, §4.1),
        // with a third event going back to the *first* run — so a reader
        // that walked runs in order would emit these out of sequence.
        // Only one `task_id`-scoped `(created_at, id)` order gets it right.
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
    async fn list_for_task_page_caps_results_and_pages_via_the_cursor() {
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
        let run = seed_task_run_for_task(&pool, &task_id, "chatting").await;

        let mut events = Vec::new();
        for n in 0..5 {
            events.push(
                append(&pool, &run, EventType::AssistantMessage, json!({ "n": n }))
                    .await
                    .unwrap(),
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let page1 = list_for_task_page(&pool, &task_id, None, 2).await.unwrap();
        assert_eq!(
            page1.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![events[0].id.clone(), events[1].id.clone()]
        );

        let cursor = (page1[1].created_at, page1[1].id.clone());
        let page2 = list_for_task_page(&pool, &task_id, Some(&cursor), 2)
            .await
            .unwrap();
        assert_eq!(
            page2.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![events[2].id.clone(), events[3].id.clone()]
        );

        let cursor = (page2[1].created_at, page2[1].id.clone());
        let page3 = list_for_task_page(&pool, &task_id, Some(&cursor), 2)
            .await
            .unwrap();
        assert_eq!(
            page3.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![events[4].id.clone()]
        );
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

    /// A reply arrives as one event per text block, so the blocks have to be
    /// rejoined in order before a `capture: json` turn can parse them (#45) —
    /// and nothing that isn't the agent speaking may leak into that text.
    #[tokio::test]
    async fn assistant_text_joins_only_the_agents_own_blocks_in_order() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;

        for (event_type, payload) in [
            (EventType::HumanMessage, json!({ "text": "review this" })),
            (
                EventType::AssistantMessage,
                json!({ "text": "{\"outcome\":" }),
            ),
            (
                EventType::ToolResult,
                json!({ "tool": "Read", "output": "a huge file" }),
            ),
            (
                EventType::AssistantMessage,
                json!({ "text": " \"approved\"}" }),
            ),
        ] {
            append(&pool, &task_run_id, event_type, payload)
                .await
                .unwrap();
        }

        let text = assistant_text_for_run(&pool, &task_run_id).await.unwrap();
        assert_eq!(text, "{\"outcome\":\n \"approved\"}");
    }

    #[tokio::test]
    async fn assistant_text_is_empty_for_a_run_that_said_nothing() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        assert_eq!(
            assistant_text_for_run(&pool, &task_run_id).await.unwrap(),
            ""
        );
    }
}
