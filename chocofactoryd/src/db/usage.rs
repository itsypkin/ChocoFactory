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
    .bind(to_i64(usage.tokens.input))
    .bind(to_i64(usage.tokens.output))
    .bind(to_i64(usage.tokens.cache_read))
    .bind(to_i64(usage.tokens.cache_write))
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
