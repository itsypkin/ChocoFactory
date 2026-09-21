use sqlx::SqlitePool;

use crate::adapter::AgentHandle;
use crate::db::events;

/// Drains `handle`'s normalized event stream into the `events` table,
/// append-only, per design §4.2. Returns once the stream ends — the
/// underlying process exited, whether that's a one-turn `agent_turn`
/// stage completing on its own or a chat session being torn down — or a
/// write fails.
pub async fn capture_events(
    pool: &SqlitePool,
    session_id: &str,
    handle: &mut AgentHandle,
) -> Result<(), sqlx::Error> {
    while let Some(event) = handle.recv().await {
        events::append(pool, session_id, event.event_type(), event.payload()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chocofactory_core::models::EventType;
    use serde_json::json;

    use super::*;
    use crate::adapter::{AgentAdapter, ClaudeAdapter, RoleConfig};
    use crate::db::{connect_in_memory, projects, sessions, tasks};

    async fn seed_session(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        let task_id = tasks::create(
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
        .id;
        sessions::create(
            pool,
            sessions::NewSession {
                task_id: &task_id,
                stage: "coding",
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap()
        .id
    }

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn drains_a_one_turn_agent_session_into_the_events_table() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;

        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_oneshot.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: crate::adapter::Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("hello", &cfg).unwrap();

        capture_events(&pool, &session_id, &mut handle)
            .await
            .unwrap();

        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let types: Vec<EventType> = stored.iter().map(|e| e.event_type).collect();
        // `fake_claude_oneshot.py` emits a `result` line after its reply
        // (#70), which now normalizes to `TurnCompleted` rather than being
        // discarded.
        assert_eq!(
            types,
            vec![
                EventType::SessionMeta,
                EventType::AssistantMessage,
                EventType::TurnCompleted
            ]
        );
        assert_eq!(stored[1].payload["text"], "echo:hello");
        // Both rows resolve back to the session they were captured from.
        assert!(
            stored
                .iter()
                .all(|e| e.session_id.as_deref() == Some(session_id.as_str()))
        );
    }
}
