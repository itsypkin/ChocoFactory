//! Task create/list/status and send-message handlers (P1-9, design §6.2:
//! `choco task create`/`list`/`status`/`send`).

use std::path::PathBuf;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chocofactory_core::models::{Event, RetryMode, RetryOutcome, Task, TaskSummary, WorkflowState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ApiError, AppState};
use crate::adapter;
use crate::db::usage as usage_db;
use crate::db::{events, tasks, workflow_state};
use crate::engine::{ConfigPatchError, WorkflowEngine, WorkflowRef};
use crate::usage::{self, TaskTimes, TaskUsage};

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub project_id: String,
    /// A workflow name (repo copy, else built-in). Exactly one of this and
    /// `workflow_file` must be present.
    #[serde(default)]
    pub workflow_def: Option<String>,
    /// An absolute path to a workflow `.yaml` file (#129).
    #[serde(default)]
    pub workflow_file: Option<String>,
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
    let workflow = match (body.workflow_def, body.workflow_file) {
        (Some(name), None) => WorkflowRef::Name(name),
        (None, Some(file)) => WorkflowRef::File(PathBuf::from(file)),
        _ => {
            return Err(ApiError::BadRequest(
                "pass exactly one of workflow_def and workflow_file".to_string(),
            ));
        }
    };
    let task = state
        .engine
        .create_task_from(
            &body.project_id,
            workflow,
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
    /// One status, or a comma-separated list (`open,stuck`).
    pub status: Option<String>,
    /// `id` (default) or `updated_desc`.
    pub order: Option<String>,
    /// Clamped to `1..=MAX_LIST_LIMIT`; absent means no limit.
    pub limit: Option<usize>,
}

/// Same ceiling `api/events.rs` puts on its `limit`.
const MAX_LIST_LIMIT: usize = 500;

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<Vec<TaskSummary>>, ApiError> {
    let statuses: Vec<String> = match query.status.as_deref() {
        None => Vec::new(),
        Some(raw) => raw
            .split(',')
            .map(|item| {
                let item = item.trim();
                if item.is_empty() {
                    Err(ApiError::BadRequest(format!(
                        "empty item in status '{raw}'"
                    )))
                } else {
                    Ok(item.to_string())
                }
            })
            .collect::<Result<_, _>>()?,
    };
    let order = match query.order.as_deref() {
        None | Some("id") => tasks::SummaryOrder::Id,
        Some("updated_desc") => tasks::SummaryOrder::UpdatedDesc,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unknown order '{other}': expected 'id' or 'updated_desc'"
            )));
        }
    };
    let limit = query.limit.map(|l| l.clamp(1, MAX_LIST_LIMIT) as i64);
    let rows = tasks::list_summaries(
        &state.pool,
        query.project_id.as_deref(),
        &statuses,
        order,
        limit,
    )
    .await?;
    Ok(Json(rows))
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
    /// Where a task cancelled with `--keep` left its work (#102); `None`
    /// for every other task.
    pub kept: Option<KeptWork>,
    /// What the task has cost and how long it ran; `null` when no turn has
    /// recorded usage.
    pub usage: Option<TaskUsage>,
    /// Set when the task is parked at a gate because a watcher timed out
    /// (#179): which stage stopped watching, after how long, and where a
    /// note goes. `null` otherwise.
    pub watch_timed_out: Option<crate::engine::WatchTimedOutInfo>,
}

/// The worktree path and branch a `cancel --keep` handed to a person.
#[derive(Serialize)]
pub struct KeptWork {
    pub worktree_path: Option<String>,
    pub branch: String,
}

fn kept_work(task: &Task) -> Option<KeptWork> {
    if !task.kept_work {
        return None;
    }
    // No worktree snapshot means no worktree and no branch were ever
    // created, so there is nothing that was kept.
    let (Some(repo), Some(project)) = (&task.worktree_repo, &task.worktree_project) else {
        return None;
    };
    let worktree_path =
        crate::worktree::worktree_path(std::path::Path::new(repo), project, &task.id)
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
    Some(KeptWork {
        worktree_path,
        branch: crate::worktree::branch_name(&task.id),
    })
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
fn workflow_file_status(engine: &WorkflowEngine, task: &Task) -> Option<&'static str> {
    let recorded = task.workflow_path.as_deref()?;
    // A `builtin:<name>@<version>` record is not a path: re-hash the
    // daemon's current copy of that built-in instead.
    let builtin_file =
        crate::engine::parse_builtin_ref(recorded).map(|name| engine.builtin_workflow_file(name));
    let path = match &builtin_file {
        Some(file) => file.display().to_string(),
        None => recorded.to_string(),
    };
    let path = path.as_str();
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
async fn read_usage(
    state: &AppState,
    id: &str,
    task: &chocofactory_core::models::Task,
    stage_trail: &[chocofactory_core::models::Event],
) -> Result<Option<TaskUsage>, sqlx::Error> {
    // Rows first: every row's session exists, so the session list read
    // afterwards can only be a superset of what the rows refer to.
    let usage_rows = usage_db::list_rows_for_task(&state.pool, id).await?;
    let usage_sessions = usage_db::list_session_facts(&state.pool, id).await?;
    Ok(usage::aggregate(
        TaskTimes {
            status: &task.status,
            created_at: task.created_at,
            updated_at: task.updated_at,
        },
        &usage_sessions,
        &usage_rows,
        stage_trail,
        chrono::Utc::now(),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    let task = tasks::get(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such task '{id}'")))?;
    let workflow_state = workflow_state::get(&state.pool, &id).await?;
    let stage_trail = events::list_stage_trail(&state.pool, &id).await?;
    let workflow_file_status = workflow_file_status(&state.engine, &task);
    let kept = kept_work(&task);
    // Usage is an add-on: a read that fails (say, an undecodable row) is
    // logged and the task is returned with `usage: null`, so the status and
    // the dashboard detail never depend on it.
    let usage = match read_usage(&state, &id, &task, &stage_trail).await {
        Ok(usage) => usage,
        Err(err) => {
            tracing::error!(task_id = %id, %err, "failed to read task usage");
            None
        }
    };
    let watch_timed_out = match &workflow_state {
        Some(ws) => state.engine.watch_timed_out(&task, ws).await,
        None => None,
    };
    Ok(Json(TaskDetail {
        usage,
        watch_timed_out,
        task,
        kept,
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
    adapter::check_task_config_clis(&body.config, state.engine.registry())
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;
    // A patch that points a role at a CLI is checked against the task's
    // workflow before anything is merged. Only a body that sets a string
    // `roles.<name>.cli` can change which adapter a role runs on.
    if sets_a_role_cli(&body.config) {
        match state.engine.check_config_patch(&id, &body.config).await {
            Ok(()) => {}
            Err(ConfigPatchError::Rejected(message)) => {
                return Err(ApiError::BadRequest(message));
            }
            Err(ConfigPatchError::Db(err)) => return Err(err.into()),
        }
    }
    let task = tasks::merge_config(&state.pool, &id, body.config)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such task '{id}'")))?;
    Ok(Json(task))
}

/// Whether `config` sets any string `roles.<name>.cli`.
fn sets_a_role_cli(config: &serde_json::Value) -> bool {
    config
        .get("roles")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|roles| {
            roles
                .values()
                .any(|role| role.get("cli").is_some_and(serde_json::Value::is_string))
        })
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

/// The optional body of `POST …/cancel` (#102). Absent, `{}` and `{"keep":
/// false}` all mean "clean up as before"; `keep: true` leaves the worktree
/// and branch in place for a person to take over.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CancelBody {
    keep: bool,
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
    body: Option<Json<CancelBody>>,
) -> Result<StatusCode, ApiError> {
    let keep = body.map(|Json(body)| body.keep).unwrap_or_default();
    state.engine.cancel_task(&id, keep).await?;
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
fn tests_support_bad_cli_yaml() -> &'static str {
    "name: bad\nroles:\n  chat:\n    cli: cluade\n    model: sonnet\nstages:\n  chatting:\n    kind: agent_turn\n    role: chat\n    on: {}\n"
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

    /// A task written straight to the database (no engine, so no session of
    /// its own racing the test), with one session and one usage turn.
    async fn seed_task_with_usage(server: &TestServer, with_usage: bool) -> String {
        use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};
        use crate::db::{projects, sessions, tasks, usage};

        let pool = server.pool();
        let project = projects::create(pool, &format!("p{}", uuid::Uuid::new_v4()), None)
            .await
            .unwrap();
        let task = tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project.id,
                workflow_def: "chat",
                title: "t",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
            },
        )
        .await
        .unwrap();
        if with_usage {
            let session = sessions::create(
                pool,
                sessions::NewSession {
                    task_id: &task.id,
                    stage: "implement",
                    role: "coder",
                    cli_adapter: "claude",
                    model: "sonnet",
                },
            )
            .await
            .unwrap();
            let turn = TurnUsage {
                cost_usd: Some(0.09),
                tokens: TokenCounts {
                    input: Some(30),
                    output: Some(15),
                    cache_read: Some(300),
                    cache_write: Some(60),
                },
                models: None,
                wall_time_ms: Some(1),
                model_turns: Some(1),
                billing: BillingMode::Subscription,
                counting: UsageCounting::CumulativePerConversation,
            };
            usage::append_turn_completed(pool, &session.id, json!({ "is_error": false }), &turn)
                .await
                .unwrap();
        }
        task.id
    }

    #[tokio::test]
    async fn get_task_carries_usage_and_list_rows_carry_the_total() {
        let server = TestServer::start().await;
        let with = seed_task_with_usage(&server, true).await;
        let without = seed_task_with_usage(&server, false).await;

        let detail: Value = server.get(&format!("/tasks/{with}")).await.json();
        let usage = &detail["usage"];
        assert!((usage["cost_usd"].as_f64().unwrap() - 0.09).abs() < 1e-9);
        assert_eq!(usage["billing_label"], "api_equivalent");
        assert_eq!(
            usage["tokens"],
            json!({"input": 30, "output": 15, "cache_read": 300, "cache_write": 60})
        );
        assert!(usage["wall_time_ms"].is_i64());
        assert_eq!(usage["active_time_ms"], Value::Null);
        assert_eq!(usage["sessions_without_data"], 0);
        assert_eq!(usage["by_stage"][0]["stage"], "implement");
        assert_eq!(usage["by_role"][0]["role"], "coder");
        assert_eq!(usage["by_lap"][0]["lap"], 1);
        assert_eq!(usage["by_model"], json!([]));

        let detail: Value = server.get(&format!("/tasks/{without}")).await.json();
        assert_eq!(detail["usage"], Value::Null);

        let list: Value = server.get("/tasks").await.json();
        let row = |id: &str| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id)
                .unwrap()
                .clone()
        };
        let total = &row(&with)["usage_total"];
        assert!((total["cost_usd"].as_f64().unwrap() - 0.09).abs() < 1e-9);
        assert_eq!(total["tokens"], 405);
        assert_eq!(total["billing_label"], "api_equivalent");
        assert_eq!(row(&without)["usage_total"], Value::Null);
    }

    #[tokio::test]
    async fn a_turn_without_a_cost_is_counted_in_the_detail_and_the_list_total() {
        use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};
        let server = TestServer::start().await;
        let id = seed_task_with_usage(&server, true).await;
        let session: String = sqlx::query_scalar("SELECT id FROM sessions WHERE task_id = ?")
            .bind(&id)
            .fetch_one(server.pool())
            .await
            .unwrap();
        let turn = TurnUsage {
            cost_usd: None,
            tokens: TokenCounts {
                input: Some(1),
                output: Some(1),
                cache_read: Some(1),
                cache_write: Some(1),
            },
            models: None,
            wall_time_ms: None,
            model_turns: None,
            billing: BillingMode::Subscription,
            counting: UsageCounting::PerTurn,
        };
        crate::db::usage::append_turn_completed(
            server.pool(),
            &session,
            json!({ "is_error": false }),
            &turn,
        )
        .await
        .unwrap();
        let detail: Value = server.get(&format!("/tasks/{id}")).await.json();
        assert_eq!(detail["usage"]["turns_without_cost"], 1);
        let list: Value = server.get("/tasks").await.json();
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id.as_str())
            .unwrap();
        assert_eq!(row["usage_total"]["turns_without_cost"], 1);
    }

    #[tokio::test]
    async fn an_undecodable_usage_row_still_returns_the_task_with_null_usage() {
        let server = TestServer::start().await;
        let id = seed_task_with_usage(&server, true).await;
        sqlx::query("UPDATE turn_usage SET models = 'not json' WHERE task_id = ?")
            .bind(&id)
            .execute(server.pool())
            .await
            .unwrap();
        let resp = server.get(&format!("/tasks/{id}")).await;
        assert_eq!(resp.status(), 200);
        let detail: Value = resp.json();
        assert_eq!(detail["id"], id.as_str());
        assert_eq!(detail["usage"], Value::Null);
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
        crate::test_support::wait_until(
            &format!("the follow-up echo on task {task_id} (follow-up reaching the live session)"),
            || async {
                let events = crate::db::events::list_for_task(server.pool(), &task_id)
                    .await
                    .unwrap();
                if events.iter().any(|e| {
                    e.payload
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t == "echo:again")
                }) {
                    Ok(())
                } else {
                    let texts: Vec<String> = events
                        .iter()
                        .rev()
                        .take(5)
                        .map(|e| format!("{}:{:?}", e.event_type, e.payload.get("text")))
                        .collect();
                    Err(format!("{} events, newest first: {texts:?}", events.len()))
                }
            },
        )
        .await;
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
        crate::test_support::wait_until(
            &format!("task {task_id} to reach its terminal stage 'finished'"),
            || async {
                let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
                if detail["workflow_state"]["current_stage"] == "finished" {
                    Ok(())
                } else {
                    Err(format!(
                        "stage {}",
                        detail["workflow_state"]["current_stage"]
                    ))
                }
            },
        )
        .await;

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

    /// `POST …/cancel` takes an optional body (#102): no body, `{}` and
    /// `{"keep": false}` are the old behaviour; `{"keep": true}` also
    /// records the flag, readable on the task with the kept branch.
    #[tokio::test]
    async fn cancel_body_is_optional_and_keep_is_recorded() {
        let server = TestServer::start().await;
        for body in [None, Some(json!({})), Some(json!({"keep": false}))] {
            let task_id = chat_task(&server).await;
            let path = format!("/tasks/{task_id}/cancel");
            let response = match body {
                None => server.post_empty(&path).await,
                Some(body) => server.post(&path, body).await,
            };
            assert_eq!(response.status(), 202);
            let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
            assert_eq!(detail["status"], "cancelled");
            assert_eq!(detail["kept_work"], false);
            assert!(detail["kept"].is_null());
        }

        let task_id = chat_task(&server).await;
        let response = server
            .post(&format!("/tasks/{task_id}/cancel"), json!({"keep": true}))
            .await;
        assert_eq!(response.status(), 202);
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["status"], "cancelled");
        assert_eq!(detail["kept_work"], true);
        // A chat task never had a worktree, so nothing was kept.
        assert!(detail["kept"].is_null());
    }

    /// With a worktree snapshot, `kept` names the worktree path and branch.
    #[tokio::test]
    async fn keep_cancel_on_a_worktree_task_exposes_the_kept_path_and_branch() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        crate::db::tasks::set_worktree(server.pool(), &task_id, "/tmp/some-repo", "demo")
            .await
            .unwrap();
        let response = server
            .post(&format!("/tasks/{task_id}/cancel"), json!({"keep": true}))
            .await;
        assert_eq!(response.status(), 202);
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        let expected = crate::worktree::worktree_path(
            std::path::Path::new("/tmp/some-repo"),
            "demo",
            &task_id,
        )
        .unwrap();
        assert_eq!(detail["kept_work"], true);
        assert_eq!(
            detail["kept"]["worktree_path"],
            expected.to_string_lossy().as_ref()
        );
        assert_eq!(detail["kept"]["branch"], format!("task/{task_id}"));
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
    /// subprocess from the persisted `adapter_session_id` — restarting the agent
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

    // ---- #164: summary rows, status lists, order, limit ----

    async fn make_task(server: &TestServer, project_id: &str, title: &str) -> String {
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "gate-only",
                    "title": title,
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        task["id"].as_str().unwrap().to_string()
    }

    fn seed_gate_only(server: &TestServer) {
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
    }

    fn ids(listed: &Value) -> Vec<String> {
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn list_rows_carry_workflow_facts_and_the_pull_request() {
        let server = TestServer::start().await;
        seed_gate_only(&server);
        let project_id = create_project(&server).await;
        let with_pr = make_task(&server, &project_id, "with pr").await;
        let without = make_task(&server, &project_id, "without").await;
        let odd = make_task(&server, &project_id, "odd url").await;

        let set_payload = |id: String, payload: Value| {
            let pool = server.pool().clone();
            async move {
                sqlx::query(
                    "UPDATE workflow_state SET payload = ?, loop_counters = ? WHERE task_id = ?",
                )
                .bind(payload.to_string())
                .bind(json!({"gate": {"count": 2}}).to_string())
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        set_payload(
            with_pr.clone(),
            json!({"stages": {
                "plain": "text",
                "review": {"text": "huge", "url": "https://example.com/build/1"},
                "open_pr": {"url": "https://github.com/o/r/pull/171"},
            }}),
        )
        .await;
        set_payload(
            odd.clone(),
            json!({"stages": {"open_pr": {"url": "https://github.com/o/r/pull/abc"}}}),
        )
        .await;

        let listed: Value = server.get("/tasks").await.json();
        let rows = listed.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let row = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap();

        let r = row(&with_pr);
        assert_eq!(r["current_stage"], "gate");
        assert!(r["stage_entered_at"].is_string());
        assert_eq!(r["loop_counters"], json!({"gate": {"count": 2}}));
        assert_eq!(
            r["pr"],
            json!({"number": 171, "url": "https://github.com/o/r/pull/171"})
        );
        assert_eq!(r["title"], "with pr");
        assert!(row(&without)["pr"].is_null());
        assert!(row(&odd)["pr"].is_null());
    }

    #[tokio::test]
    async fn list_status_accepts_a_comma_separated_list() {
        let server = TestServer::start().await;
        seed_gate_only(&server);
        let project_id = create_project(&server).await;
        let open = make_task(&server, &project_id, "a").await;
        let stuck = make_task(&server, &project_id, "b").await;
        let cancelled = make_task(&server, &project_id, "c").await;
        crate::db::tasks::mark_stuck(server.pool(), &stuck, "x")
            .await
            .unwrap();
        server
            .post(&format!("/tasks/{cancelled}/cancel"), json!({}))
            .await;

        let both: Value = server.get("/tasks?status=open,%20stuck").await.json();
        let mut got = ids(&both);
        got.sort();
        let mut want = vec![open.clone(), stuck.clone()];
        want.sort();
        assert_eq!(got, want);

        let single: Value = server.get("/tasks?status=cancelled").await.json();
        assert_eq!(ids(&single), vec![cancelled]);

        assert_eq!(server.get("/tasks?status=open,").await.status(), 400);
    }

    #[tokio::test]
    async fn list_order_and_limit() {
        let server = TestServer::start().await;
        seed_gate_only(&server);
        let project_id = create_project(&server).await;
        let a = make_task(&server, &project_id, "a").await;
        let b = make_task(&server, &project_id, "b").await;
        let c = make_task(&server, &project_id, "c").await;
        for (id, ts) in [
            (&a, "2026-01-01T00:00:03+00:00"),
            (&b, "2026-01-01T00:00:01+00:00"),
            (&c, "2026-01-01T00:00:02+00:00"),
        ] {
            sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
                .bind(ts)
                .bind(id)
                .execute(server.pool())
                .await
                .unwrap();
        }

        let listed: Value = server.get("/tasks?order=updated_desc&limit=2").await.json();
        assert_eq!(ids(&listed), vec![a.clone(), c.clone()]);

        let by_id: Value = server.get("/tasks?order=id").await.json();
        let mut sorted = vec![a, b, c];
        sorted.sort();
        assert_eq!(ids(&by_id), sorted);

        let zero: Value = server.get("/tasks?limit=0").await.json();
        assert_eq!(ids(&zero).len(), 1, "limit clamps up to 1");

        let bad = server.get("/tasks?order=bogus").await;
        assert_eq!(bad.status(), 400);
        assert!(bad.json().to_string().contains("updated_desc"));
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
        assert!(outcome["adapter_session_id"].is_null());

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

    /// Mirrors `cancelled_tasks_are_filterable_by_status`: `--status stuck`
    /// needs no DB-layer change either, but nothing pinned the round trip
    /// for it before now (#61).
    #[tokio::test]
    async fn stuck_tasks_are_filterable_by_status() {
        let server = TestServer::start().await;
        let task_id = stuck_gate_task(&server).await;

        let listed: Value = server.get("/tasks?status=stuck").await.json();
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
    /// on disk (issue #88), for a task running an explicit workflow file.
    #[tokio::test]
    async fn get_task_workflow_file_status_tracks_edits_and_deletion() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let file_dir = server.temp_dir();
        let workflow_path = file_dir.join("mine.yaml");
        std::fs::copy(server.builtin_workflow_path("chat"), &workflow_path).unwrap();
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_file": workflow_path.to_str().unwrap(),
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "unchanged");
        let workflow_path = detail["workflow_path"]
            .as_str()
            .expect("the task has a recorded workflow_path")
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

    /// The same walk for a built-in task (#129): `workflow_path` is a
    /// `builtin:` record, and the status re-hashes the daemon's current copy.
    #[tokio::test]
    async fn get_builtin_task_workflow_file_status_tracks_the_builtin_copy() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;

        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        let recorded = detail["workflow_path"].as_str().unwrap();
        assert!(recorded.starts_with("builtin:chat@"), "{recorded}");
        assert_eq!(detail["workflow_file_status"], "unchanged");

        let file = server.builtin_workflow_path("chat");
        let original = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{original}\n# edited\n")).unwrap();
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "changed");

        std::fs::remove_file(&file).unwrap();
        let detail: Value = server.get(&format!("/tasks/{task_id}")).await.json();
        assert_eq!(detail["workflow_file_status"], "missing");
    }

    /// A built-in that vanished from the daemon's copy is a conflict, not a
    /// server error, for both send and retry.
    #[tokio::test]
    async fn a_vanished_builtin_is_a_conflict_on_send_and_retry() {
        let server = TestServer::start().await;
        let task_id = chat_task(&server).await;
        std::fs::remove_file(server.builtin_workflow_path("chat")).unwrap();
        let send = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "hi" }),
            )
            .await;
        assert_eq!(send.status(), 409, "{}", send.json());
        assert!(
            send.json()
                .to_string()
                .contains("is not part of this version"),
            "{}",
            send.json()
        );
        // Retry only reaches the workflow load for a stuck task.
        crate::db::tasks::mark_stuck(server.pool(), &task_id, "stage 'chatting': it broke")
            .await
            .unwrap();
        let retry = server
            .post(&format!("/tasks/{task_id}/retry"), json!({}))
            .await;
        assert_eq!(retry.status(), 409, "{}", retry.json());
        assert!(
            retry
                .json()
                .to_string()
                .contains("is not part of this version"),
            "{}",
            retry.json()
        );
    }

    /// `POST /tasks` with `workflow_file` (#129): an absolute path is
    /// recorded canonically; both or neither field, a relative path and a
    /// missing file are all refused.
    #[tokio::test]
    async fn create_task_with_a_workflow_file() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let file = server.temp_dir().join("wf.yaml");
        std::fs::copy(server.builtin_workflow_path("chat"), &file).unwrap();
        let file_str = file.to_str().unwrap();

        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_file": file_str,
                    "title": "t",
                    "prompt": "hi",
                }),
            )
            .await;
        assert_eq!(response.status(), 201, "{}", response.json());
        let task = response.json();
        assert_eq!(
            task["workflow_path"],
            std::fs::canonicalize(&file).unwrap().to_str().unwrap()
        );

        for body in [
            json!({"project_id": project_id, "workflow_def": "chat", "workflow_file": file_str,
                   "title": "t", "prompt": "hi"}),
            json!({"project_id": project_id, "title": "t", "prompt": "hi"}),
            json!({"project_id": project_id, "workflow_file": "rel/wf.yaml",
                   "title": "t", "prompt": "hi"}),
        ] {
            let response = server.post("/tasks", body).await;
            assert_eq!(response.status(), 400, "{}", response.json());
        }
        let both = server
            .post(
                "/tasks",
                json!({"project_id": project_id, "title": "t", "prompt": "hi"}),
            )
            .await;
        assert!(
            both.json()["error"]
                .as_str()
                .unwrap()
                .contains("pass exactly one of workflow_def and workflow_file")
        );

        let missing = server.temp_dir().join("nope.yaml");
        let response = server
            .post(
                "/tasks",
                json!({"project_id": project_id, "workflow_file": missing.to_str().unwrap(),
                       "title": "t", "prompt": "hi"}),
            )
            .await;
        assert_eq!(response.status(), 500);
        assert!(
            response.json()["error"]
                .as_str()
                .unwrap()
                .contains(missing.to_str().unwrap())
        );
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

    // ---- #175: waiting_on_human and verdict markers ----

    #[tokio::test]
    async fn list_reports_waiting_on_human_only_for_an_open_task_at_a_gate() {
        let server = TestServer::start().await;
        seed_gate_only(&server);
        let project_id = create_project(&server).await;
        let at_gate = make_task(&server, &project_id, "gate").await;
        let at_poll = make_task(&server, &project_id, "poll").await;
        let stuck = make_task(&server, &project_id, "stuck").await;
        let unknown = make_task(&server, &project_id, "null kind").await;
        let no_row = make_task(&server, &project_id, "no row").await;

        let pool = server.pool().clone();
        sqlx::query("UPDATE workflow_state SET stage_kind = 'poll' WHERE task_id = ?")
            .bind(&at_poll)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET status = 'stuck', stuck_reason = 'x' WHERE id = ?")
            .bind(&stuck)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
            .bind(&unknown)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workflow_state WHERE task_id = ?")
            .bind(&no_row)
            .execute(&pool)
            .await
            .unwrap();

        let listed: Value = server.get("/tasks").await.json();
        let waiting = |id: &str| {
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .unwrap()["waiting_on_human"]
                .clone()
        };
        assert_eq!(waiting(&at_gate), json!(true));
        assert_eq!(waiting(&at_poll), json!(false));
        assert_eq!(waiting(&stuck), json!(false));
        assert_eq!(waiting(&unknown), json!(false));
        assert_eq!(waiting(&no_row), json!(false));
    }

    fn seed_marker_gate(server: &TestServer) {
        server.write_workflow(
            "marker-gate",
            r#"
name: marker-gate
stages:
  gate:
    kind: human_gate
    capture: text
    markers:
      - line: /request-changes
        then: changes_requested
      - line: /approve
        then: approved
    on: { approved: done, changes_requested: done }
  done:
    kind: terminal
"#,
        );
    }

    async fn make_marker_task(server: &TestServer, project_id: &str) -> String {
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "marker-gate",
                    "title": "t",
                    "prompt": "hello",
                }),
            )
            .await
            .json();
        task["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_reply_with_a_marker_is_accepted_and_one_without_is_a_400() {
        let server = TestServer::start().await;
        seed_marker_gate(&server);
        let project_id = create_project(&server).await;

        let accepted = make_marker_task(&server, &project_id).await;
        let response = server
            .post(
                &format!("/tasks/{accepted}/messages"),
                json!({ "text": "/approve" }),
            )
            .await;
        assert_eq!(response.status(), 202, "{}", response.json());
        let task: Value = server.get(&format!("/tasks/{accepted}")).await.json();
        assert_eq!(task["workflow_state"]["current_stage"], "done");

        let refused = make_marker_task(&server, &project_id).await;
        let response = server
            .post(
                &format!("/tasks/{refused}/messages"),
                json!({ "text": "ok" }),
            )
            .await;
        assert_eq!(response.status(), 400);
        let error = response.json()["error"].as_str().unwrap().to_string();
        assert!(
            error.contains("/request-changes") && error.contains("/approve"),
            "{error}"
        );
        assert!(error.contains("Nothing was sent."), "{error}");
        let task: Value = server.get(&format!("/tasks/{refused}")).await.json();
        assert_eq!(task["workflow_state"]["current_stage"], "gate");

        let conflict = make_marker_task(&server, &project_id).await;
        let response = server
            .post(
                &format!("/tasks/{conflict}/messages"),
                json!({ "text": "/approve\n/request-changes" }),
            )
            .await;
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json()["error"],
            "your reply has both /request-changes and /approve; keep one. Nothing was sent."
        );
    }

    // ---- #166: an unknown `cli:` is rejected ----

    #[tokio::test]
    async fn create_task_with_an_unknown_cli_in_its_config_is_400_and_creates_nothing() {
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
                    "prompt": "hi",
                    "config": { "cwd": ".", "roles": { "coder": { "cli": "nope" } } },
                }),
            )
            .await;
        assert_eq!(response.status(), 400, "{}", response.json());
        let error = response.json()["error"].as_str().unwrap().to_string();
        assert!(
            error.contains("'nope'") && error.contains("known CLIs: claude"),
            "{error}"
        );
        let listed = server.get(&format!("/tasks?project_id={project_id}")).await;
        assert_eq!(listed.json().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_task_from_a_workflow_file_with_an_unknown_cli_is_400() {
        let server = TestServer::start().await;
        let project_id = create_project(&server).await;
        let file = server.temp_dir().join("bad.yaml");
        std::fs::write(&file, super::tests_support_bad_cli_yaml()).unwrap();
        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_file": file.to_str().unwrap(),
                    "title": "t",
                    "prompt": "hi",
                }),
            )
            .await;
        assert_eq!(response.status(), 400, "{}", response.json());
        assert_eq!(
            response.json()["error"],
            "role 'chat' uses cli 'cluade', which this daemon doesn't know; known CLIs: claude"
        );
    }

    #[tokio::test]
    async fn patch_task_config_with_an_unknown_cli_is_400_and_leaves_the_config_alone() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project_id = create_project(&server).await;
        let task_id = create_two_role_task(&server, &project_id).await;
        let before = server.get(&format!("/tasks/{task_id}")).await.json()["config"].clone();

        let response = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": { "coder": { "cli": "nope" } } } }),
            )
            .await;
        assert_eq!(response.status(), 400, "{}", response.json());
        let error = response.json()["error"].as_str().unwrap().to_string();
        assert!(
            error.contains("'nope'") && error.contains("known CLIs: claude"),
            "{error}"
        );
        let after = server.get(&format!("/tasks/{task_id}")).await.json()["config"].clone();
        assert_eq!(before, after);

        // A non-string `cli` still falls through, as it always has.
        let response = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": { "coder": { "cli": 1 } } } }),
            )
            .await;
        assert_eq!(response.status(), 200, "{}", response.json());
        assert_eq!(response.json()["config"]["roles"]["coder"]["cli"], 1);
    }

    #[tokio::test]
    async fn patch_task_config_pointing_a_memory_role_at_omp_is_400_and_leaves_the_config_alone() {
        let server = TestServer::start_with_omp().await;
        server.write_workflow(
            "memflow",
            "name: memflow\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n    memory: true\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: {}\n",
        );
        let project_id = create_project(&server).await;
        let task: Value = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "memflow",
                    "title": "t",
                    "prompt": "hello",
                    "config": { "cwd": "." },
                }),
            )
            .await
            .json();
        let task_id = task["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{task}"))
            .to_string();
        let before = server.get(&format!("/tasks/{task_id}")).await.json()["config"].clone();

        let response = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": { "coder": { "cli": "omp" } } } }),
            )
            .await;
        assert_eq!(response.status(), 400, "{}", response.json());
        assert_eq!(
            response.json()["error"],
            "role 'coder' runs on cli 'omp', which can't use memory: true; remove memory: true \
             or run the role on cli: claude"
        );
        let after = server.get(&format!("/tasks/{task_id}")).await.json()["config"].clone();
        assert_eq!(before, after);

        // Claude with memory, a role the workflow doesn't define, and a
        // body that sets no cli at all are all fine.
        for config in [
            json!({ "roles": { "coder": { "cli": "claude" } } }),
            json!({ "roles": { "nobody": { "cli": "omp" } } }),
            json!({ "roles": { "coder": { "model": "opus" } } }),
        ] {
            let response = server
                .patch(&format!("/tasks/{task_id}"), json!({ "config": config }))
                .await;
            assert_eq!(response.status(), 200, "{config}: {}", response.json());
        }

        // A task that doesn't exist is a 404, not a skipped check.
        let response = server
            .patch(
                "/tasks/no-such-task",
                json!({ "config": { "roles": { "coder": { "cli": "omp" } } } }),
            )
            .await;
        assert_eq!(response.status(), 404, "{}", response.json());
        assert_eq!(response.json()["error"], "no such task 'no-such-task'");

        // A workflow that no longer loads skips the check (the turn-start
        // one still fails closed), rather than blocking every config change.
        std::fs::remove_file(server.builtin_workflow_path("memflow")).unwrap();
        let response = server
            .patch(
                &format!("/tasks/{task_id}"),
                json!({ "config": { "roles": { "coder": { "cli": "omp" } } } }),
            )
            .await;
        assert_eq!(response.status(), 200, "{}", response.json());
    }

    #[tokio::test]
    async fn a_chat_message_for_a_session_on_an_unknown_adapter_is_409() {
        let server = TestServer::start_with_adapter_binary("fake_claude_oneshot.py").await;
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
                    "config": { "cwd": "." },
                }),
            )
            .await
            .json();
        let task_id = task["id"].as_str().unwrap().to_string();
        sqlx::query("UPDATE sessions SET cli_adapter = 'ghost' WHERE task_id = ?")
            .bind(&task_id)
            .execute(server.pool())
            .await
            .unwrap();
        // The one-shot fixture exits on its own; once its slot is gone the
        // message takes the resume path and meets the unknown adapter.
        let body = crate::test_support::wait_until(
            &format!("a 409 for a message to task {task_id}"),
            || async {
                let response = server
                    .post(
                        &format!("/tasks/{task_id}/messages"),
                        json!({ "text": "hi" }),
                    )
                    .await;
                if response.status() == 409 {
                    Ok(response.json())
                } else {
                    Err(format!("{}: {}", response.status(), response.json()))
                }
            },
        )
        .await;
        assert_eq!(
            body["error"],
            "role 'chat' uses cli 'ghost', which this daemon doesn't know; known CLIs: claude"
        );
    }

    #[tokio::test]
    async fn a_cli_that_slips_in_after_startup_is_400_at_create_and_409_at_retry() {
        let dir = std::env::temp_dir().join(format!("choco-gc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.yaml");
        let server = TestServer::start_with_global_config(config_path.clone()).await;
        // No `cli:` on the role, so the global config's value is the one used.
        server.write_workflow(
            "nocli",
            "name: nocli\nroles:\n  chat:\n    model: sonnet\nstages:\n  chatting:\n    kind: agent_turn\n    role: chat\n    on: {}\n",
        );
        // Written after the engine was built, so no start-time check saw it.
        std::fs::write(&config_path, "roles:\n  chat:\n    cli: bogus\n").unwrap();
        let project_id = create_project(&server).await;
        let response = server
            .post(
                "/tasks",
                json!({
                    "project_id": project_id,
                    "workflow_def": "nocli",
                    "title": "t",
                    "prompt": "hello",
                    "config": { "cwd": "." },
                }),
            )
            .await;
        assert_eq!(response.status(), 400, "{}", response.json());
        let error = response.json()["error"].as_str().unwrap().to_string();
        assert!(error.contains("'bogus'"), "{error}");
        assert!(error.contains("known CLIs: claude"), "{error}");
        let list = server
            .get(&format!("/tasks?project_id={project_id}"))
            .await
            .json();
        let tasks = list.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{list}");
        assert_eq!(tasks[0]["status"], "stuck");
        let task_id = tasks[0]["id"].as_str().unwrap().to_string();

        let response = server
            .post(&format!("/tasks/{task_id}/retry"), json!({}))
            .await;
        assert_eq!(response.status(), 409, "{}", response.json());
        assert!(
            response.json()["error"]
                .as_str()
                .unwrap()
                .contains("known CLIs: claude"),
            "{}",
            response.json()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
