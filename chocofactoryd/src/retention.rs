use std::time::Duration;

use sqlx::SqlitePool;

use crate::db::events;

/// Config for the daily events-retention job (§4.4).
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub interval: Duration,
    pub max_age: chrono::Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(24 * 60 * 60),
            max_age: chrono::Duration::days(365),
        }
    }
}

/// Runs the retention job forever, pruning `events` rows older than
/// `config.max_age` every `config.interval`. Never touches `tasks`/
/// `sessions` (§4.4) — event detail ages out, task history doesn't.
/// Meant to be spawned as a background task by the daemon's startup code.
pub async fn run_retention_job(pool: SqlitePool, config: RetentionConfig) {
    run_loop(&pool, &config, None).await;
}

async fn run_once(pool: &SqlitePool, max_age: chrono::Duration) -> Result<u64, sqlx::Error> {
    let cutoff = chrono::Utc::now() - max_age;
    events::delete_older_than(pool, cutoff).await
}

async fn run_loop(pool: &SqlitePool, config: &RetentionConfig, max_iterations: Option<usize>) {
    let mut interval = tokio::time::interval(config.interval);
    let mut ran = 0usize;
    loop {
        interval.tick().await;
        match run_once(pool, config.max_age).await {
            Ok(pruned) if pruned > 0 => tracing::info!(pruned, "retention job: pruned old events"),
            Ok(_) => tracing::debug!("retention job: no events old enough to prune"),
            Err(err) => tracing::error!(%err, "retention job: failed to prune events"),
        }
        ran += 1;
        if max_iterations.is_some_and(|limit| ran >= limit) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::{connect_in_memory, events, projects, sessions, tasks};
    use chocofactory_core::models::EventType;

    async fn seed_session(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        let task_id = tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "chat",
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
        .id;
        sessions::create(
            pool,
            sessions::NewSession {
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
    async fn prunes_events_once_they_are_older_than_max_age() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        events::append(&pool, &session_id, EventType::Error, json!({}))
            .await
            .unwrap();

        run_loop(
            &pool,
            &RetentionConfig {
                interval: Duration::from_millis(1),
                max_age: chrono::Duration::zero(),
            },
            Some(1),
        )
        .await;

        assert!(
            events::list_for_session(&pool, &session_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn keeps_events_younger_than_max_age() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        events::append(&pool, &session_id, EventType::Error, json!({}))
            .await
            .unwrap();

        run_loop(
            &pool,
            &RetentionConfig {
                interval: Duration::from_millis(1),
                max_age: chrono::Duration::days(365),
            },
            Some(1),
        )
        .await;

        assert_eq!(
            events::list_for_session(&pool, &session_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn pruning_events_leaves_usage_totals_and_breakdowns_unchanged() {
        use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};
        use crate::db::usage;
        use crate::usage::{TaskTimes, aggregate};

        let pool = connect_in_memory().await.unwrap();
        let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
        let task = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "chat",
                title: "T",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
            },
        )
        .await
        .unwrap();
        events::append_stage_transition(&pool, &task.id, "implement", None, "agent_turn")
            .await
            .unwrap();
        let new = |stage| sessions::NewSession {
            task_id: &task.id,
            stage,
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        };
        let first = sessions::create(&pool, new("implement")).await.unwrap().id;
        // An ended session with no usage row.
        let second = sessions::create(&pool, new("review")).await.unwrap().id;
        sqlx::query("UPDATE sessions SET ended_at = ? WHERE id = ?")
            .bind(chrono::Utc::now())
            .bind(&second)
            .execute(&pool)
            .await
            .unwrap();
        let tokens = TokenCounts {
            input: Some(10),
            output: Some(5),
            cache_read: Some(100),
            cache_write: Some(20),
        };
        let turn = TurnUsage {
            cost_usd: Some(0.25),
            tokens,
            models: Some(vec![crate::adapter::ModelUsage {
                model: "mock".to_string(),
                tokens,
                cost_usd: Some(0.25),
            }]),
            wall_time_ms: Some(1),
            model_turns: Some(1),
            billing: BillingMode::Subscription,
            counting: UsageCounting::CumulativePerConversation,
        };
        usage::append_turn_completed(&pool, &first, json!({ "is_error": false }), &turn)
            .await
            .unwrap();

        let now = chrono::Utc::now() + chrono::Duration::hours(1);
        let snapshot = || async {
            let rows = usage::list_rows_for_task(&pool, &task.id).await.unwrap();
            let facts = usage::list_session_facts(&pool, &task.id).await.unwrap();
            let trail = events::list_stage_trail(&pool, &task.id).await.unwrap();
            let u = aggregate(
                TaskTimes {
                    status: "closed",
                    created_at: task.created_at,
                    updated_at: task.updated_at,
                },
                &facts,
                &rows,
                &trail,
                now,
            )
            .unwrap();
            (serde_json::to_value(&u).unwrap(), trail.len())
        };
        let (mut before, trail_before) = snapshot().await;
        assert_eq!(trail_before, 1);

        let pruned = run_once(&pool, chrono::Duration::zero()).await.unwrap();
        assert!(pruned >= 2, "the stage entry and turn_completed event go");
        let (mut after, trail_after) = snapshot().await;
        assert_eq!(trail_after, 0, "the stage trail really was pruned");

        // Active time comes from the pruned trail and is allowed to go.
        before["active_time_ms"] = json!(null);
        after["active_time_ms"] = json!(null);
        assert_eq!(before, after);
        assert_eq!(after["cost_usd"], 0.25);
        assert_eq!(after["sessions_without_data"], 1);
        assert_eq!(after["by_lap"].as_array().unwrap().len(), 2);
    }
}
