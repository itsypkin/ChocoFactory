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
/// `task_runs` (§4.4) — event detail ages out, task history doesn't.
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
        if let Err(err) = run_once(pool, config.max_age).await {
            eprintln!("retention job: failed to prune events: {err}");
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
    use crate::db::{connect_in_memory, events, projects, task_runs, tasks};
    use chokofactory_core::models::EventType;

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
    async fn prunes_events_once_they_are_older_than_max_age() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        events::append(&pool, &task_run_id, EventType::Error, json!({}))
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
            events::list_for_task_run(&pool, &task_run_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn keeps_events_younger_than_max_age() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        events::append(&pool, &task_run_id, EventType::Error, json!({}))
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
            events::list_for_task_run(&pool, &task_run_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
