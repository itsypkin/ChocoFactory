//! `turn_usage`: what each agent turn used. Written in the same transaction
//! as the `turn_completed` event it belongs to, and never pruned.

use chocofactory_core::models::{Event, EventType};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, SqliteConnection, SqlitePool};

use super::events;
use crate::adapter::{TokenCounts, TurnUsage};
use crate::usage::{self, Baseline, ModelMap, SessionFacts, UsageRow};

/// The sessions to search for a baseline: `session_id`, then its
/// `resumed_from`, and so on. Bounded and cycle-safe.
async fn session_chain(
    conn: &mut SqliteConnection,
    session_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let mut chain = vec![session_id.to_string()];
    loop {
        let parent: Option<Option<String>> =
            sqlx::query_scalar("SELECT resumed_from FROM sessions WHERE id = ?")
                .bind(chain.last().expect("chain starts non-empty"))
                .fetch_optional(&mut *conn)
                .await?;
        match parent.flatten() {
            Some(parent) if !chain.contains(&parent) => chain.push(parent),
            _ => return Ok(chain),
        }
    }
}

/// The latest non-null reported cost and reported models in the session's
/// chain, each searched independently, newest first.
async fn baseline(conn: &mut SqliteConnection, session_id: &str) -> Result<Baseline, sqlx::Error> {
    let chain = session_chain(conn, session_id).await?;
    let mut cost_usd = None;
    for id in &chain {
        cost_usd = sqlx::query_scalar::<_, f64>(
            "SELECT reported_cost_usd FROM turn_usage
             WHERE session_id = ? AND reported_cost_usd IS NOT NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        if cost_usd.is_some() {
            break;
        }
    }
    let mut models = None;
    for id in &chain {
        models = sqlx::query_scalar::<_, Json<ModelMap>>(
            "SELECT reported_models FROM turn_usage
             WHERE session_id = ? AND reported_models IS NOT NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .map(|j| j.0);
        if models.is_some() {
            break;
        }
    }
    Ok(Baseline { cost_usd, models })
}

fn to_i64(n: Option<u64>) -> Option<i64> {
    n.map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

/// Records a top-level `turn_completed` event and the turn's usage row in
/// one transaction: both commit or neither does. The event goes first so
/// SQLite takes the write lock before the baseline is read; an unknown
/// session fails there with `RowNotFound`, exactly like `events::append`.
pub async fn append_turn_completed(
    pool: &SqlitePool,
    session_id: &str,
    payload: Value,
    usage: &TurnUsage,
) -> Result<Event, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let event = events::append_on(&mut tx, session_id, EventType::TurnCompleted, payload).await?;
    let base = baseline(&mut tx, session_id).await?;
    let turn = usage::per_turn(usage, &base);
    let tokens = usage::turn_tokens(&usage.tokens, turn.models.as_ref());
    sqlx::query(
        "INSERT INTO turn_usage (task_id, session_id, recorded_at, billing, counting,
             reported_cost_usd, cost_usd, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, duration_ms, model_turns, reported_models, models)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event.task_id)
    .bind(session_id)
    .bind(Utc::now())
    .bind(usage.billing.as_str())
    .bind(usage.counting.as_str())
    .bind(usage.cost_usd)
    .bind(turn.cost_usd)
    .bind(to_i64(tokens.input))
    .bind(to_i64(tokens.output))
    .bind(to_i64(tokens.cache_read))
    .bind(to_i64(tokens.cache_write))
    .bind(to_i64(usage.wall_time_ms))
    .bind(usage.model_turns.map(i64::from))
    .bind(turn.reported_models.map(Json))
    .bind(turn.models.map(Json))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(event)
}

#[derive(FromRow)]
struct RowRecord {
    session_id: String,
    billing: String,
    cost_usd: Option<f64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    models: Option<Json<ModelMap>>,
}

fn to_u64(n: Option<i64>) -> Option<u64> {
    n.and_then(|n| u64::try_from(n).ok())
}

/// A task's usage rows, oldest first.
pub async fn list_rows_for_task(
    pool: &SqlitePool,
    task_id: &str,
) -> Result<Vec<UsageRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RowRecord>(
        "SELECT session_id, billing, cost_usd, input_tokens, output_tokens, cache_read_tokens,
                cache_write_tokens, models
         FROM turn_usage WHERE task_id = ? ORDER BY id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| UsageRow {
            session_id: r.session_id,
            billing: r.billing,
            cost_usd: r.cost_usd,
            tokens: TokenCounts {
                input: to_u64(r.input_tokens),
                output: to_u64(r.output_tokens),
                cache_read: to_u64(r.cache_read_tokens),
                cache_write: to_u64(r.cache_write_tokens),
            },
            models: r.models.map(|m| m.0),
        })
        .collect())
}

#[derive(FromRow)]
struct SessionRecord {
    id: String,
    stage: String,
    role: String,
    lap: Option<i64>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

/// A task's sessions with the facts the roll-up groups by, oldest first.
pub async fn list_session_facts(
    pool: &SqlitePool,
    task_id: &str,
) -> Result<Vec<SessionFacts>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SessionRecord>(
        "SELECT id, stage, role, lap, started_at, ended_at
         FROM sessions WHERE task_id = ? ORDER BY started_at, id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SessionFacts {
            id: r.id,
            stage: r.stage,
            role: r.role,
            lap: r.lap,
            started_at: r.started_at,
            ended: r.ended_at.is_some(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::adapter::{BillingMode, ModelUsage, UsageCounting};
    use crate::db::{connect_in_memory, projects, sessions, tasks};

    fn usage(cost: Option<f64>, input: u64, models: Option<Vec<ModelUsage>>) -> TurnUsage {
        TurnUsage {
            cost_usd: cost,
            tokens: TokenCounts {
                input: Some(input),
                output: Some(1),
                cache_read: Some(1),
                cache_write: Some(1),
            },
            models,
            wall_time_ms: Some(1000),
            model_turns: Some(1),
            billing: BillingMode::Subscription,
            counting: UsageCounting::CumulativePerConversation,
        }
    }

    fn one_model(input: u64, cost: f64) -> Option<Vec<ModelUsage>> {
        Some(vec![ModelUsage {
            model: "m".to_string(),
            tokens: TokenCounts {
                input: Some(input),
                output: None,
                cache_read: None,
                cache_write: None,
            },
            cost_usd: Some(cost),
        }])
    }

    async fn new_task(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "chat",
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

    async fn new_session(pool: &SqlitePool, task_id: &str, stage: &str) -> String {
        sessions::create(
            pool,
            sessions::NewSession {
                task_id,
                stage,
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn resumed(pool: &SqlitePool, task_id: &str, stage: &str, from: &str) -> String {
        sessions::create_resumed(
            pool,
            sessions::NewSession {
                task_id,
                stage,
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
            },
            sessions::ResumedFrom {
                session_id: from,
                adapter_session_id: "adapter",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn turn(pool: &SqlitePool, session: &str, u: &TurnUsage) {
        append_turn_completed(pool, session, json!({ "is_error": false }), u)
            .await
            .unwrap();
    }

    async fn costs(pool: &SqlitePool, task_id: &str) -> Vec<Option<f64>> {
        list_rows_for_task(pool, task_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.cost_usd)
            .collect()
    }

    fn close(a: Option<f64>, b: f64) {
        assert!((a.unwrap() - b).abs() < 1e-9, "{a:?} vs {b}");
    }

    #[tokio::test]
    async fn a_turns_tokens_are_the_sum_of_its_per_model_figures() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let s = new_session(&pool, &task, "implement").await;
        let m = |name: &str, n: u64| ModelUsage {
            model: name.to_string(),
            tokens: TokenCounts {
                input: Some(n),
                output: Some(n),
                cache_read: Some(n),
                cache_write: Some(n),
            },
            cost_usd: Some(0.01),
        };
        // `usage` holds only the main model's 10; the sub-agent's model
        // adds 5 more in the per-model figures.
        let u = usage(Some(0.02), 10, Some(vec![m("main", 10), m("sub", 5)]));
        turn(&pool, &s, &u).await;
        let rows = list_rows_for_task(&pool, &task).await.unwrap();
        let t = rows[0].tokens;
        assert_eq!(
            (t.input, t.output, t.cache_read, t.cache_write),
            (Some(15), Some(15), Some(15), Some(15))
        );
    }

    #[tokio::test]
    async fn a_second_turn_in_the_same_row_is_measured_against_the_first() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let s = new_session(&pool, &task, "implement").await;
        turn(&pool, &s, &usage(Some(0.02490), 10, one_model(10, 0.02490))).await;
        turn(&pool, &s, &usage(Some(0.02927), 10, one_model(20, 0.02927))).await;
        let c = costs(&pool, &task).await;
        close(c[0], 0.02490);
        close(c[1], 0.00437);
        let rows = list_rows_for_task(&pool, &task).await.unwrap();
        assert_eq!(rows[1].models.as_ref().unwrap()["m"].input_tokens, Some(10));
    }

    #[tokio::test]
    async fn a_resumed_session_continues_its_parents_chain() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let a = new_session(&pool, &task, "implement").await;
        turn(&pool, &a, &usage(Some(0.02927), 10, None)).await;
        let b = resumed(&pool, &task, "implement", &a).await;
        turn(&pool, &b, &usage(Some(0.03288), 10, None)).await;
        close(costs(&pool, &task).await[1], 0.00361);
    }

    #[tokio::test]
    async fn the_chain_reaches_past_a_middle_session_with_no_rows() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let a = new_session(&pool, &task, "implement").await;
        turn(&pool, &a, &usage(Some(0.02927), 10, None)).await;
        let b = resumed(&pool, &task, "implement", &a).await;
        let c = resumed(&pool, &task, "implement", &b).await;
        turn(&pool, &c, &usage(Some(0.03288), 10, None)).await;
        close(costs(&pool, &task).await[1], 0.00361);
    }

    #[tokio::test]
    async fn a_fresh_session_does_not_see_another_sessions_totals() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let a = new_session(&pool, &task, "implement").await;
        turn(&pool, &a, &usage(Some(0.5), 10, None)).await;
        let b = new_session(&pool, &task, "implement").await;
        turn(&pool, &b, &usage(Some(0.2), 10, None)).await;
        close(costs(&pool, &task).await[1], 0.2);
    }

    #[tokio::test]
    async fn a_turn_without_cost_is_skipped_by_the_next_baseline() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let a = new_session(&pool, &task, "implement").await;
        turn(&pool, &a, &usage(Some(0.1), 10, None)).await;
        turn(&pool, &a, &usage(None, 10, None)).await;
        turn(&pool, &a, &usage(Some(0.3), 10, None)).await;
        let c = costs(&pool, &task).await;
        assert_eq!(c[1], None);
        close(c[2], 0.2);
    }

    async fn count(pool: &SqlitePool, sql: &'static str, session: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(session)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    const EVENTS: &str =
        "SELECT COUNT(*) FROM events WHERE session_id = ? AND event_type = 'turn_completed'";
    const ROWS: &str = "SELECT COUNT(*) FROM turn_usage WHERE session_id = ?";

    #[tokio::test]
    async fn success_writes_exactly_one_event_and_one_row() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let s = new_session(&pool, &task, "implement").await;
        let event = append_turn_completed(
            &pool,
            &s,
            json!({ "is_error": false }),
            &usage(Some(0.1), 10, None),
        )
        .await
        .unwrap();
        assert_eq!(event.event_type, EventType::TurnCompleted);
        assert_eq!(count(&pool, EVENTS, &s).await, 1);
        assert_eq!(count(&pool, ROWS, &s).await, 1);
    }

    #[tokio::test]
    async fn a_failing_usage_insert_rolls_the_event_back() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let s = new_session(&pool, &task, "implement").await;
        sqlx::query(
            "CREATE TRIGGER forced BEFORE INSERT ON turn_usage
             BEGIN SELECT RAISE(ABORT, 'forced'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let err = append_turn_completed(
            &pool,
            &s,
            json!({ "is_error": false }),
            &usage(Some(0.1), 10, None),
        )
        .await;
        assert!(err.is_err());
        assert_eq!(count(&pool, EVENTS, &s).await, 0);
        assert_eq!(count(&pool, ROWS, &s).await, 0);
    }

    #[tokio::test]
    async fn an_unknown_session_fails_and_leaves_no_row() {
        let pool = connect_in_memory().await.unwrap();
        let err = append_turn_completed(
            &pool,
            "nope",
            json!({ "is_error": false }),
            &usage(Some(0.1), 10, None),
        )
        .await;
        assert!(matches!(err, Err(sqlx::Error::RowNotFound)));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_usage")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    async fn enter(pool: &SqlitePool, task: &str, stage: &str, outcome: Option<&str>) {
        events::append_stage_transition(pool, task, stage, outcome, "agent_turn")
            .await
            .unwrap();
    }

    async fn lap(pool: &SqlitePool, session: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT lap FROM sessions WHERE id = ?")
            .bind(session)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn laps_count_entries_not_retries_and_resumes_keep_their_lap() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        enter(&pool, &task, "implement", None).await;
        let a = new_session(&pool, &task, "implement").await;
        assert_eq!(lap(&pool, &a).await, Some(1));

        enter(&pool, &task, "implement", Some("retry")).await;
        let b = new_session(&pool, &task, "implement").await;
        assert_eq!(lap(&pool, &b).await, Some(1));
        let c = resumed(&pool, &task, "implement", &b).await;
        assert_eq!(lap(&pool, &c).await, Some(1));

        enter(&pool, &task, "review", Some("done")).await;
        enter(&pool, &task, "implement", Some("changes_requested")).await;
        let d = new_session(&pool, &task, "implement").await;
        assert_eq!(lap(&pool, &d).await, Some(2));
        // A resume of the first lap's session stays in lap 1.
        let e = resumed(&pool, &task, "implement", &a).await;
        assert_eq!(lap(&pool, &e).await, Some(1));
    }

    #[tokio::test]
    async fn a_resumed_retry_entry_does_not_start_a_new_lap() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        enter(&pool, &task, "implement", None).await;
        let a = new_session(&pool, &task, "implement").await;
        enter(&pool, &task, "implement", Some("retry_resume")).await;
        let b = resumed(&pool, &task, "implement", &a).await;
        assert_eq!(lap(&pool, &b).await, Some(1));
        enter(&pool, &task, "review", Some("done")).await;
        enter(&pool, &task, "implement", Some("changes_requested")).await;
        let d = new_session(&pool, &task, "implement").await;
        assert_eq!(lap(&pool, &d).await, Some(2));
    }

    #[tokio::test]
    async fn a_session_with_no_stage_entries_is_lap_one() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        let a = new_session(&pool, &task, "implement").await;
        assert_eq!(lap(&pool, &a).await, Some(1));
    }

    #[tokio::test]
    async fn stage_transitions_record_the_stage_kind() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        events::append_stage_transition(&pool, &task, "gate", None, "human_gate")
            .await
            .unwrap();
        let trail = events::list_stage_trail(&pool, &task).await.unwrap();
        assert_eq!(trail[0].payload["kind"], "human_gate");
    }
}
