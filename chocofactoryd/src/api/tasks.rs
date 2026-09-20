//! Task create/list/status and send-message handlers (P1-9, design §6.2:
//! `choco task create`/`list`/`status`/`send`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chocofactory_core::models::{Event, RetryMode, RetryOutcome, Task, WorkflowState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ApiError, AppState};
use crate::db::{events, tasks, workflow_state};

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
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<Task>), ApiError> {
    let task = state
        .engine
        .create_task(
            &body.project_id,
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

/// A task plus its current `workflow_state` — bare `Task.status` is
/// `"open"`/`"closed"` (§5.4), `"cancelled"` (#69), or `"stuck"` (X-4,
/// issue #61, which also carries a `stuck_reason` via `Task`'s own flattened
/// fields), so `choco task status <id>` needs `current_stage` too for this
/// to actually be useful as a status view.
#[derive(Serialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub workflow_state: Option<WorkflowState>,
    /// Every stage this task has entered, oldest first — the replacement
    /// for `workflow_state.stage_history`, which X-3 removed in favour of
    /// the events timeline. Served from here rather than left to the caller
    /// to filter out of `GET /tasks/:id/events` so `choco task status`
    /// stays one request and can't show a page-truncated trail.
    pub stage_trail: Vec<Event>,
    /// Whether `task.workflow_path`'s contents still match the recorded
    /// `workflow_sha256` (issue #88) — `"unchanged"`, `"changed"`, or
    /// `"missing"` when the file can't be re-read right now (deleted,
    /// permissions, ...; any I/O error collapses to this rather than
    /// failing the whole request). `None` when the task has no
    /// `workflow_path` at all (a legacy task, predating this column).
    /// Computed fresh on every request by re-hashing the file — not cached,
    /// same as every other "resolved right now" value this API serves.
    pub workflow_file_status: Option<&'static str>,
}

/// Computes [`TaskDetail::workflow_file_status`] for `task`. A pure
/// function of the file on disk right now — re-hashes it and compares
/// against `task.workflow_sha256` — so it's unit-testable without a running
/// server.
///
/// Any read error collapses to `"missing"` so a stale/deleted file never
/// fails the whole `GET /tasks/{id}` request, but the error itself is still
/// logged with the task id and path (issue #88 review, F3) — silently
/// discarding it would violate this repo's own rule that every I/O failure
/// is propagated or logged with context, and would print a misleading
/// "(missing)" for a file that actually exists but, say, hit `EACCES`.
fn workflow_file_status(task: &Task) -> Option<&'static str> {
    let path = task.workflow_path.as_deref()?;
    let status = match std::fs::read(path) {
        Ok(bytes) => {
            if Some(crate::engine::sha256_hex(&bytes).as_str()) == task.workflow_sha256.as_deref() {
                "unchanged"
            } else {
                "changed"
            }
        }
        Err(err) => {
            tracing::warn!(
                task_id = %task.id, %path, %err,
                "failed to re-read a task's recorded workflow file for workflow_file_status; \
                 reporting \"missing\""
            );
            "missing"
        }
    };
    Some(status)
}

/// Three separate reads, deliberately not one transaction: a transition
/// committing between them can hand back a `current_stage` and a
/// `stage_trail` that disagree by one hop in either direction, since the
/// engine updates `workflow_state` and appends the `stage_entered` event as
/// two writes.
///
/// Both skews are harmless *because the renderer never assumes they agree*:
/// it marks the last trail entry as current only when it actually matches
/// `current_stage`, and otherwise names the current stage on its own line.
/// A poll one moment later shows the settled pair. Wrapping this in a
/// transaction would not fix it either — the engine's own two writes aren't
/// atomic, so the skew is in the data, not in the read.
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    let task = tasks::get(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such task '{id}'")))?;
    let workflow_state = workflow_state::get(&state.pool, &id).await?;
    let stage_trail = events::list_stage_trail(&state.pool, &id).await?;
    let workflow_file_status = workflow_file_status(&task);
    Ok(Json(TaskDetail {
        task,
        workflow_state,
        stage_trail,
        workflow_file_status,
    }))
}

#[derive(Deserialize)]
pub struct UpdateTaskConfigRequest {
    pub config: Value,
}

/// Merges `config` into the task's existing config (P2-6, §5.5) — the
/// task-level layer `role_config::resolve` reads, so this is how a role's
/// `cli`/`model`/`system_prompt` gets changed after creation.
///
/// Merge rather than replace so overriding one role leaves the task-wide
/// `cwd` and every other role alone; `db::tasks::merge_config` does it in a
/// single statement so concurrent patches can't lose each other's keys.
///
/// A non-object `config` is rejected here, before the DB call: `json_patch`
/// would treat a scalar or array as a wholesale replacement of the column,
/// silently wiping every role. `create` above deliberately doesn't make the
/// same check — it *establishes* a task's config rather than merging into an
/// existing one, so an odd shape there destroys nothing, and `resolve` reads
/// through it as "no overrides"
/// (`role_config::tests::malformed_task_config_falls_through_instead_of_erroring`).
///
/// This is *not* the same check as validating what's *inside* `config.roles` —
/// per the P1-8 LLD, an unknown role name or a wrong-typed field there
/// deliberately means "not overridden" rather than an error, and that leniency
/// is preserved (same test).
///
/// Takes effect on the task's **next** turn: `resolve` re-reads
/// `task.config` on every `enter_agent_turn`/`send_message` and caches
/// nothing, so an in-flight session keeps the config it started with.
pub async fn update_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskConfigRequest>,
) -> Result<Json<Task>, ApiError> {
    if !body.config.is_object() {
        return Err(ApiError::BadRequest(
            "'config' must be a JSON object".to_string(),
        ));
    }
    let task = tasks::merge_config(&state.pool, &id, body.config)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such task '{id}'")))?;
    Ok(Json(task))
}

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
}

/// Relays a human message into `id`'s current stage — an open `agent_turn`
/// or a `human_gate`'s resume — via `WorkflowEngine::send_message_or_resume`
/// (P1-9). The actual reply, if any, arrives over `/tasks/:id/events`
/// (paginated) or `/tasks/:id/events/live` (WS), not in this response.
pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<StatusCode, ApiError> {
    state.engine.send_message_or_resume(&id, &body.text).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Stops `id` for good (#69): marks it `cancelled`, kills whatever agent
/// subprocess group it had running, and removes its worktree.
///
/// `POST …/cancel` rather than `DELETE /tasks/{id}` because this ends the
/// task's *work*, not the task's *record* — the row, its events, and the
/// stage it stopped in all remain readable afterwards, which is most of
/// the point of cancelling rather than deleting.
///
/// `202`, not `200`: the kill is a signal. By the time this returns the
/// task is durably un-advanceable and the signal is delivered, but the
/// subprocess's own teardown — final events draining into the timeline,
/// the session landing on `exited` — completes just after. Poll
/// `GET /tasks/{id}` for the settled state.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.engine.cancel_task(&id).await?;
    Ok(StatusCode::ACCEPTED)
}

/// The body carries the retry's `mode` (#92). Optional, and absent means
/// `auto`, so a caller that predates the flag — or one that has no opinion —
/// keeps posting `{}` and gets resume-when-safe.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RetryBody {
    mode: RetryMode,
}

/// Reopens a `stuck` task and re-enters whatever stage it stopped in (X-4,
/// issue #61), resuming that stage's interrupted agent session when `mode`
/// allows and there is one worth resuming, and starting a fresh one
/// otherwise (#92).
///
/// `202`, not `200`, for the same reason `cancel` is: by the time this
/// returns the stage has been re-entered, but a `shell`/`poll`/`agent_turn`
/// stage's actual work — the command running, the turn's session starting —
/// continues detached. Poll `GET /tasks/{id}` for the settled state. Unlike
/// `cancel`'s, this `202` carries a body, since which of resume and fresh
/// happened is not something the caller can infer from the status alone.
pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<RetryBody>>,
) -> Result<(StatusCode, Json<RetryOutcome>), ApiError> {
    let mode = body.map(|Json(body)| body.mode).unwrap_or_default();
    let outcome = state.engine.retry_task(&id, mode).await?;
    Ok((StatusCode::ACCEPTED, Json(outcome)))
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

    /// P2-6: several roles configured independently on one task, in one
    /// request. The daemon stores `config` verbatim, so this pins that the
    /// role-keyed shape survives the round trip for more than one role —
    /// `choco`'s flags and `role_config::resolve` both depend on it.
    ///
    /// `cwd` is `"."` rather than a made-up path because it's the directory
    /// the agent subprocess is actually spawned in: a non-existent one fails
    /// the spawn with a 500 and tells you nothing about config round-tripping.
    #[tokio::test]
    async fn create_task_round_trips_config_for_more_than_one_role() {
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
                    "config": {
                        "cwd": ".",
                        "roles": {
                            "coder": { "model": "opus" },
                            "reviewer": { "model": "sonnet", "cli": "claude" }
                        }
                    },
                }),
            )
            .await;
        assert_eq!(response.status(), 201, "body: {}", response.json());
        let task = response.json();
        assert_eq!(task["config"]["cwd"], ".");
        assert_eq!(task["config"]["roles"]["coder"]["model"], "opus");
        assert_eq!(task["config"]["roles"]["reviewer"]["model"], "sonnet");
        assert_eq!(task["config"]["roles"]["reviewer"]["cli"], "claude");
    }

    /// Creates a task with two configured roles and returns its id.
    async fn create_two_role_task(server: &TestServer, project_id: &str) -> String {
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "chat",
                    "title": "t",
                    "prompt": "hello",
                    "config": {
                        "cwd": ".",
                        "roles": {
                            "coder": { "model": "sonnet", "cli": "claude" },
                            "reviewer": { "model": "sonnet" }
                        }
                    },
                }),
            )
            .await
            .json();
        task["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn patch_task_config_merges_rather_than_replacing() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task_id = create_two_role_task(&server, &project_id).await;

        let response = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": { "coder": { "model": "opus" } } } }),
            )
            .await;
        assert_eq!(response.status(), 200);
        let task = response.json();

        assert_eq!(task["config"]["roles"]["coder"]["model"], "opus");
        // Everything the patch didn't mention survives.
        assert_eq!(task["config"]["cwd"], ".");
        assert_eq!(task["config"]["roles"]["coder"]["cli"], "claude");
        assert_eq!(task["config"]["roles"]["reviewer"]["model"], "sonnet");
    }

    /// Two roles reconfigured in a single patch — the edit-side counterpart
    /// to `create_task_round_trips_config_for_more_than_one_role`.
    #[tokio::test]
    async fn patch_task_config_can_change_more_than_one_role_at_once() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task_id = create_two_role_task(&server, &project_id).await;

        let task: Value = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": {
                    "coder": { "model": "opus" },
                    "reviewer": { "model": "haiku" }
                } } }),
            )
            .await
            .json();

        assert_eq!(task["config"]["roles"]["coder"]["model"], "opus");
        assert_eq!(task["config"]["roles"]["reviewer"]["model"], "haiku");
        assert_eq!(task["config"]["roles"]["coder"]["cli"], "claude");
    }

    /// A scalar or array would make `json_patch` replace the whole column,
    /// wiping every role — so it's a 400 before the DB is touched, not a
    /// silent data loss.
    #[tokio::test]
    async fn patch_task_config_rejects_a_non_object_config() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task_id = create_two_role_task(&server, &project_id).await;

        for bad in [json!("nope"), json!([1, 2]), json!(7), json!(null)] {
            let response = server
                .patch(&format!("/tasks/{task_id}"), json!({ "config": bad }))
                .await;
            assert_eq!(response.status(), 400, "expected 400 for config {bad}");
        }

        // The rejected patches left the task untouched.
        let task: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(task["config"]["roles"]["coder"]["model"], "sonnet");
        assert_eq!(task["config"]["cwd"], ".");
    }

    #[tokio::test]
    async fn patch_task_config_on_an_unknown_task_is_404() {
        let server = TestServer::start().await;

        let response = server
            .patch("/tasks/no-such-task", json!({ "config": { "cwd": "." } }))
            .await;
        assert_eq!(response.status(), 404);
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

    /// A `workflow_def` outside the allowlist must be rejected as a 400,
    /// not silently joined onto the project's repo and global workflow
    /// directories and searched for as if it were a normal name (issue #88
    /// review, F1) — `resolve_task_workflow`'s allowlist check is the only
    /// thing standing between this endpoint and path traversal now that the
    /// name is joined onto a caller-controlled `project.repo_path` as well
    /// as the global directory.
    #[tokio::test]
    async fn create_task_with_an_invalid_workflow_name_is_400() {
        let server = TestServer::start().await;
        let project_id = create_project(&server).await;

        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "../etc/passwd",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await;
        assert_eq!(response.status(), 400);
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

    // ---- cancel (#69) ----

    /// Creates a `chat` task with a live session and returns its id.
    async fn chat_task(server: &TestServer) -> String {
        server.seed_chat_workflow();
        let project_id = create_project(server).await;
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
        task["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn cancel_accepts_and_marks_the_task_cancelled() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        let response = server
            .post(&format!("/tasks/{task_id}/cancel"), json!({}))
            .await;
        assert_eq!(response.status(), 202);

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["status"], "cancelled");
        // The stage the task stopped in is still readable — that's the
        // difference between cancelling a task and deleting it.
        assert_eq!(detail["workflow_state"]["current_stage"], "chatting");
    }

    #[tokio::test]
    async fn cancelling_an_unknown_task_is_404() {
        let server = TestServer::start().await;
        let response = server.post("/tasks/no-such-task/cancel", json!({})).await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn cancelling_an_already_cancelled_task_is_409() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        assert_eq!(
            server
                .post(&format!("/tasks/{task_id}/cancel"), json!({}))
                .await
                .status(),
            202
        );
        assert_eq!(
            server
                .post(&format!("/tasks/{task_id}/cancel"), json!({}))
                .await
                .status(),
            409
        );
    }

    /// The hole #69 closes at the HTTP layer: `chatting` is a
    /// standing-open `agent_turn`, so before this guard existed a message
    /// to a cancelled task would be accepted and would resume a fresh
    /// subprocess from the persisted `session_id` — restarting the agent
    /// the operator had just stopped.
    #[tokio::test]
    async fn sending_a_message_to_a_cancelled_task_is_409() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        server
            .post(&format!("/tasks/{task_id}/cancel"), json!({}))
            .await;

        let response = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "are you still there" }),
            )
            .await;
        assert_eq!(response.status(), 409);
    }

    /// `--status cancelled` needs no DB-layer change — `tasks::list`
    /// already filters on an arbitrary string — but nothing wrote that
    /// value before, so this pins the round trip.
    #[tokio::test]
    async fn cancelled_tasks_are_filterable_by_status() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        server
            .post(&format!("/tasks/{task_id}/cancel"), json!({}))
            .await;

        let listed: Value = server.get("/tasks?status=cancelled").await.json();
        let ids: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![task_id.as_str()]);

        let open: Value = server.get("/tasks?status=open").await.json();
        assert!(open.as_array().unwrap().is_empty());
    }

    // ---- X-4: stuck tasks and retry (#61) ----

    #[tokio::test]
    async fn retrying_an_unknown_task_is_404() {
        let server = TestServer::start().await;
        let response = server.post("/tasks/no-such-task/retry", json!({})).await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn retrying_a_non_stuck_task_is_409() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        let response = server
            .post(&format!("/tasks/{task_id}/retry"), json!({}))
            .await;
        assert_eq!(response.status(), 409);
    }

    /// A stuck task whose stage is a `human_gate` rather than `chat_task`'s
    /// agent_turn: it opens no session, so `retry_task`'s defensive
    /// `RunStillActive` check can't trip on a session these tests never
    /// stopped — what they exercise is the HTTP layer, not the engine.
    async fn stuck_gate_task(server: &TestServer) -> String {
        server.write_workflow(
            "gate-only",
            r#"
name: gate-only
stages:
  gate:
    kind: human_gate
    on: { resumed: done }
  done:
    kind: terminal
"#,
        );
        let project_id = create_project(server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "gate-only",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'gate': it broke")
            .await
            .unwrap();
        task_id
    }

    #[tokio::test]
    async fn retrying_a_stuck_task_is_202() {
        let server = TestServer::start().await;
        let task_id = stuck_gate_task(&server).await;

        let response = server
            .post(&format!("/tasks/{task_id}/retry"), json!({}))
            .await;
        assert_eq!(response.status(), 202, "body: {}", response.json());
        // #92: the body says what the retry did. A `human_gate` has no
        // session, so this one started fresh.
        let outcome = response.json();
        assert_eq!(outcome["stage"], "gate");
        assert_eq!(outcome["resumed"], false);
        assert!(outcome["session_id"].is_null());

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["status"], "open");
        assert!(detail["stuck_reason"].is_null());
    }

    /// #92: `"fresh"` on the wire reaches the engine as `RetryMode::Fresh`.
    /// Pinned here because every other test drives the Rust enum directly,
    /// so a rename of the serde spelling would otherwise only break the CLI.
    #[tokio::test]
    async fn an_explicit_fresh_mode_is_accepted() {
        let server = TestServer::start().await;
        let task_id = stuck_gate_task(&server).await;

        let response = server
            .post(
                &format!("/tasks/{task_id}/retry"),
                json!({ "mode": "fresh" }),
            )
            .await;
        assert_eq!(response.status(), 202, "body: {}", response.json());
        let outcome = response.json();
        assert_eq!(outcome["resumed"], false);
        assert_eq!(outcome["fresh_reason"], "a fresh start was asked for");
    }

    /// #92: `--resume` against a task whose stage has nothing resumable is
    /// refused, rather than quietly doing the other thing. The task stays
    /// stuck, so the operator can decide.
    #[tokio::test]
    async fn retrying_with_resume_when_nothing_can_be_resumed_is_409() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'chatting': it broke")
            .await
            .unwrap();

        let response = server
            .post(
                &format!("/tasks/{task_id}/retry"),
                json!({ "mode": "resume" }),
            )
            .await;
        assert_eq!(response.status(), 409, "body: {}", response.json());

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["status"], "stuck");
    }

    /// A `mode` the daemon doesn't know is a bad request, not a silent
    /// fall back to `auto` — a misspelled `--resume` must not look like it
    /// worked. (The absent-`mode` default is covered by
    /// `retrying_a_stuck_task_is_202`, which posts `{}`.)
    #[tokio::test]
    async fn an_unknown_retry_mode_is_rejected_rather_than_treated_as_auto() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'chatting': it broke")
            .await
            .unwrap();

        let response = server
            .post(
                &format!("/tasks/{task_id}/retry"),
                json!({ "mode": "sideways" }),
            )
            .await;
        let status = response.status();
        assert!(
            (400..500).contains(&status),
            "a misspelled mode must not silently fall back: {status}"
        );
    }

    #[tokio::test]
    async fn get_task_includes_the_stuck_reason() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'chatting': it broke")
            .await
            .unwrap();

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["status"], "stuck");
        assert_eq!(detail["stuck_reason"], "stage 'chatting': it broke");
    }

    #[tokio::test]
    async fn sending_a_message_to_a_stuck_task_is_409() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'chatting': it broke")
            .await
            .unwrap();

        let response = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "hello?" }),
            )
            .await;
        assert_eq!(response.status(), 409);
    }

    // ---- workflow_file_status (issue #88) ----

    /// `GET /tasks/{id}`'s `workflow_file_status` walks unchanged -> changed
    /// -> missing as the recorded workflow file is edited and then deleted
    /// on disk — the main user-visible piece of issue #88's §4, and until
    /// now entirely untested (`grep workflow_file_status` found no test).
    #[tokio::test]
    async fn get_task_workflow_file_status_tracks_edits_and_deletion() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "unchanged");
        let workflow_path = detail["workflow_path"]
            .as_str()
            .expect("chat_task's task has a recorded workflow_path")
            .to_string();

        // Editing the file on disk (without touching the task's recorded
        // hash) must flip status to "changed", not silently stay
        // "unchanged" or fail the request.
        let original = std::fs::read_to_string(&workflow_path).unwrap();
        std::fs::write(&workflow_path, format!("{original}\n# edited\n")).unwrap();
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "changed");

        // Deleting it must report "missing", not error the whole request
        // (any read error collapses to "missing" per the doc comment on
        // `workflow_file_status`).
        std::fs::remove_file(&workflow_path).unwrap();
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "missing");
    }

    /// A legacy task with no recorded `workflow_path` (predating issue #88)
    /// reports `workflow_file_status: null` rather than a bogus status.
    #[tokio::test]
    async fn get_task_workflow_file_status_is_null_without_a_recorded_path() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        // Simulate a legacy row: clear the columns issue #88 added.
        sqlx::query("UPDATE tasks SET workflow_path = NULL, workflow_sha256 = NULL WHERE id = ?")
            .bind(&task_id)
            .execute(server.pool())
            .await
            .unwrap();

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert!(detail["workflow_file_status"].is_null());
    }
}
