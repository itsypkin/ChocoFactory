//! Task create/list/status and send-message handlers (P1-9, design §6.2:
//! `choco task create`/`list`/`status`/`send`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chokofactory_core::models::{Task, WorkflowState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ApiError, AppState};
use crate::db::{tasks, workflow_state};

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub project_id: String,
    pub workflow_def: String,
    pub title: String,
    /// The task's initial human-typed message (§5.4) — becomes the entry
    /// stage's first input if it's a `prompt_file`-less `agent_turn`.
    pub prompt: String,
    #[serde(default)]
    pub config: Option<Value>,
    /// Tags this task as spawned via delegation (§6.2's
    /// `choco task create --parent-task <id>`).
    #[serde(default)]
    pub parent_task_id: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<Task>), ApiError> {
    let task = state
        .engine
        .create_task(
            &body.project_id,
            body.parent_task_id.as_deref(),
            &body.workflow_def,
            &body.title,
            &body.prompt,
            body.config.unwrap_or_else(|| json!({})),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(task)))
}

#[derive(Deserialize)]
pub struct ListTasksQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<Vec<Task>>, ApiError> {
    let tasks = tasks::list(
        &state.pool,
        query.project_id.as_deref(),
        query.status.as_deref(),
    )
    .await?;
    Ok(Json(tasks))
}

/// A task plus its current `workflow_state` — bare `Task.status` is only
/// ever `"open"`/`"closed"` (§5.4), so `choco task status <id>` needs
/// `current_stage` too for this to actually be useful as a status view.
#[derive(Serialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub workflow_state: Option<WorkflowState>,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    let task = tasks::get(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such task '{id}'")))?;
    let workflow_state = workflow_state::get(&state.pool, &id).await?;
    Ok(Json(TaskDetail {
        task,
        workflow_state,
    }))
}

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
}

/// Relays a human message into `id`'s current stage — an open `agent_turn`
/// or a `human_gate`'s resume — via `WorkflowEngine::send_message_or_resume`
/// (P1-9). The actual reply, if any, arrives over `/tasks/:id/events`, not
/// in this response.
pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<StatusCode, ApiError> {
    state.engine.send_message_or_resume(&id, &body.text).await?;
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::tests::TestServer;

    async fn create_project(server: &TestServer) -> String {
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        project["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn create_task_resolves_the_named_workflow_and_starts_it() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;

        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "chat",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await;
        assert_eq!(response.status(), 201);
        let task = response.json();
        assert_eq!(task["workflow_def"], "chat");
        assert_eq!(task["project_id"], project_id);
    }

    #[tokio::test]
    async fn create_task_with_unknown_workflow_is_404() {
        let server = TestServer::start().await;
        let project_id = create_project(&server).await;

        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "ghost",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn create_task_with_a_nonexistent_project_id_is_404_not_500() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();

        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": "no-such-project",
                    "workflow_def": "chat",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn get_task_returns_task_and_workflow_state() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "chat",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap();

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["id"], task_id);
        assert_eq!(detail["workflow_state"]["current_stage"], "chatting");
    }

    #[tokio::test]
    async fn get_unknown_task_is_404() {
        let server = TestServer::start().await;
        let status = server.get("/tasks/does-not-exist").await.status();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn send_message_reaches_the_live_session() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "chat",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();

        let response = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "again" }),
            )
            .await;
        assert_eq!(response.status(), 202);

        // fake_claude.py echoes each line it receives as `echo:<text>` in
        // an assistant message event — proves the HTTP send-message
        // handler actually reached the live session
        // `send_message_or_resume` dispatches to, not just that it
        // returned 202. Checked at the DB layer here (live-over-WS
        // delivery is `ws.rs`'s own test's job).
        let mut saw_echo = false;
        for _ in 0..200 {
            let events = crate::db::events::list_for_task(server.pool(), &task_id)
                .await
                .unwrap();
            if events.iter().any(|e| {
                e.payload
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "echo:again")
            }) {
                saw_echo = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(saw_echo, "follow-up message never reached the live session");
    }

    #[tokio::test]
    async fn send_message_to_unknown_task_is_404() {
        let server = TestServer::start().await;
        let response = server
            .post("/tasks/does-not-exist/messages", json!({ "text": "hi" }))
            .await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn send_message_to_a_terminal_task_is_409() {
        let server = TestServer::start_with_adapter_binary("fake_claude_oneshot.py").await;
        let workflows_dir_yaml = r#"
name: one-shot
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: { done: finished }
  finished:
    kind: terminal
"#;
        server.write_workflow("one-shot", workflows_dir_yaml);
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "one-shot",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();

        // fake_claude_oneshot.py exits cleanly right away, auto-advancing
        // this single-shot turn to "finished" (terminal, §5.2).
        let mut is_finished = false;
        for _ in 0..200 {
            let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
            if detail["workflow_state"]["current_stage"] == "finished" {
                is_finished = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_finished, "task never reached its terminal stage");

        let response = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "too late" }),
            )
            .await;
        assert_eq!(response.status(), 409);
    }

    /// Regression test for the P1-9 review: two overlapping resumes of the
    /// same `human_gate` (a plausible double-click/retry against a UI's
    /// "resume" button) must not surface as a 500. `advance()`'s per-task
    /// lock means `workflow_state` itself is never corrupted — the race is
    /// serialized, not lost — but the loser's `advance("resumed")` call
    /// then runs against whatever stage the winner already transitioned
    /// to, which doesn't have "resumed" as a valid outcome (`review`'s
    /// `on:` map below is `approved`/`changes_requested`), and that should
    /// map to 409 (a benign "already resumed" conflict), not 500.
    ///
    /// This doesn't depend on true thread-scheduling luck: regardless of
    /// whether the second call's initial (unlocked) stage read still sees
    /// "gate" or already sees "review", `review` is a `human_gate` too and
    /// neither has "resumed" in its `on:` map, so the second call
    /// converges on the same `UnknownOutcome` -> 409 mapping either way.
    #[tokio::test]
    async fn concurrent_resumes_of_the_same_human_gate_do_not_500() {
        let server = TestServer::start().await;
        server.write_workflow(
            "gated-relay",
            r#"
name: gated-relay
stages:
  gate:
    kind: human_gate
    on: { resumed: review }
  review:
    kind: human_gate
    on:
      approved: done
      changes_requested: gate
  done:
    kind: terminal
"#,
        );
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "gated-relay",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();

        let send = |text: &'static str| {
            let server = &server;
            let task_id = task_id.clone();
            async move {
                server
                    .post(
                        &format!("/tasks/{task_id}/messages"),
                        json!({ "text": text }),
                    )
                    .await
            }
        };
        let (first, second) = tokio::join!(send("go"), send("go"));

        let mut statuses = [first.status(), second.status()];
        statuses.sort_unstable();
        assert_eq!(
            statuses,
            [202, 409],
            "expected exactly one resume to win (202) and the other to conflict (409), got {statuses:?}"
        );
    }
}
