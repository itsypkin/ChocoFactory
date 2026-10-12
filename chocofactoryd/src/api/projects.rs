//! Project CRUD handlers (P1-9, design §6.2: `choco project create`/`list`),
//! plus `repo_path` (issue #88) and `init-workflows`.

use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use chocofactory_core::models::Project;
use serde::{Deserialize, Serialize};

use super::{ApiError, AppState};
use crate::db::{projects, tasks};
use crate::serde_util::deserialize_some;

/// `repo_path` must be an absolute path to a directory that exists on this
/// machine right now (issue #88) — checked here, in the API layer, for both
/// `POST /projects` and `PATCH /projects/{id}`. Not required to be a git
/// repo: nothing here reads `.git`, and the README is explicit that
/// registering a repo on a project means trusting whatever `.chocofactory/`
/// it contains, not that it has to look like a checkout. Not canonicalized
/// server-side either — `choco` canonicalizes client-side (the daemon's cwd
/// isn't the user's), so the API layer only validates what it's given.
fn validate_repo_path(repo_path: &str) -> Result<(), ApiError> {
    let path = Path::new(repo_path);
    if !path.is_absolute() {
        return Err(ApiError::BadRequest(format!(
            "repo_path '{repo_path}' must be an absolute path"
        )));
    }
    if !path.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "repo_path '{repo_path}' does not exist or is not a directory"
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub repo_path: Option<String>,
    /// The project's own cap on agent turns running at once; `0` or absent:
    /// the project shares the server's cap.
    #[serde(default)]
    pub max_agents: Option<u32>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<Project>), ApiError> {
    if let Some(repo_path) = &body.repo_path {
        validate_repo_path(repo_path)?;
    }
    let project = projects::create_capped(
        &state.pool,
        &body.name,
        body.repo_path.as_deref(),
        body.max_agents,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<Project>>, ApiError> {
    let projects = projects::list(&state.pool).await?;
    Ok(Json(projects))
}

pub async fn get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Project>, ApiError> {
    let project = projects::get(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such project '{id}'")))?;
    Ok(Json(project))
}

/// `PATCH /projects/{id}` body (issue #88 replaces the old bare-`name`
/// rename with this): `name` absent leaves it unchanged; `repo_path`
/// absent leaves it unchanged, `repo_path: null` clears it, and
/// `repo_path: "<path>"` sets it. Existing clients that only ever sent
/// `{"name": "..."}` keep working unchanged.
#[derive(Deserialize)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub repo_path: Option<Option<String>>,
    /// Absent: unchanged. `0` or `null`: remove the project's own cap.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub max_agents: Option<Option<u32>>,
}

pub async fn update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> Result<Json<Project>, ApiError> {
    if body.name.is_none() && body.repo_path.is_none() && body.max_agents.is_none() {
        return Err(ApiError::BadRequest(
            "PATCH /projects/{id} requires at least one of 'name'/'repo_path'/'max_agents'"
                .to_string(),
        ));
    }
    if let Some(Some(repo_path)) = &body.repo_path {
        validate_repo_path(repo_path)?;
    }
    let repo_path = body.repo_path.as_ref().map(|inner| inner.as_deref());
    let project = projects::update_capped(
        &state.pool,
        &id,
        body.name.as_deref(),
        repo_path,
        body.max_agents,
    )
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("no such project '{id}'")))?;
    if body.max_agents.is_some() {
        // A raised or removed cap, or a project leaving the server pool, can
        // let queued turns start.
        state.engine.wake_dispatcher();
    }
    Ok(Json(project))
}

/// 409 if the project still has tasks, rather than letting the delete hit
/// `tasks.project_id`'s foreign key (`db::pool::connect` enables
/// `foreign_keys`, and there's no `ON DELETE CASCADE` on that reference) —
/// a pre-check here reports the actual reason in a stable, backend-agnostic
/// shape instead of depending on how sqlx surfaces a FK violation.
pub async fn delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if projects::get(&state.pool, &id).await?.is_none() {
        return Err(ApiError::NotFound(format!("no such project '{id}'")));
    }
    let existing_tasks = tasks::list(&state.pool, Some(&id), None).await?;
    if !existing_tasks.is_empty() {
        return Err(ApiError::Conflict(format!(
            "project '{id}' still has {} task(s)",
            existing_tasks.len()
        )));
    }
    projects::delete(&state.pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /projects/{id}/init-workflows` response (issue #88): the directory
/// seeded into, plus which files were newly created versus already present
/// — the same shape `WorkflowEngine::init_project_workflows` returns, with
/// `PathBuf`s rendered as display strings for JSON.
#[derive(Serialize)]
pub struct InitWorkflowsResponse {
    pub dir: String,
    pub created: Vec<String>,
    pub existing: Vec<String>,
}

pub async fn init_workflows(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<InitWorkflowsResponse>, ApiError> {
    let report = state.engine.init_project_workflows(&id).await?;
    // The seeded directory itself isn't part of `SeedReport`, but every
    // entry in `created`/`existing` lives directly under it, so the first
    // one (there's always at least one — `chat.yaml`) gives it back without
    // recomputing `<repo_path>/.chocofactory/workflows` here too.
    let dir = report
        .created
        .iter()
        .chain(report.existing.iter())
        .next()
        .and_then(|p| p.parent())
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(Json(InitWorkflowsResponse {
        dir,
        created: report
            .created
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        existing: report
            .existing
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::tests::TestServer;

    #[tokio::test]
    async fn crud_round_trip_over_http() {
        let server = TestServer::start().await;

        let created: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        assert_eq!(created["name"], "demo");
        let id = created["id"].as_str().unwrap().to_string();

        let fetched: Value = server.get(&format!("/projects/{id}")).await.json();
        assert_eq!(fetched["id"], id);

        let listed: Value = server.get("/projects").await.json();
        assert_eq!(listed.as_array().unwrap().len(), 1);

        let renamed: Value = server
            .patch(&format!("/projects/{id}"), json!({ "name": "renamed" }))
            .await
            .json();
        assert_eq!(renamed["name"], "renamed");

        let status = server.delete(&format!("/projects/{id}")).await.status();
        assert_eq!(status, 204);

        let status = server.get(&format!("/projects/{id}")).await.status();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn deleting_a_project_with_tasks_is_rejected() {
        let server = TestServer::start().await;

        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap().to_string();
        server.seed_chat_workflow();
        server
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

        let response = server.delete(&format!("/projects/{project_id}")).await;
        assert_eq!(response.status(), 409);
    }

    #[tokio::test]
    async fn get_unknown_project_is_404() {
        let server = TestServer::start().await;
        let status = server.get("/projects/does-not-exist").await.status();
        assert_eq!(status, 404);
    }

    // ---- repo_path (issue #88) ----

    #[tokio::test]
    async fn create_with_a_valid_repo_path_round_trips_it() {
        let server = TestServer::start().await;
        let dir = server.temp_dir();

        let response = server
            .post(
                "/projects",
                json!({ "name": "demo", "repo_path": dir.to_string_lossy() }),
            )
            .await;
        assert_eq!(response.status(), 201, "body: {}", response.json());
        let project = response.json();
        assert_eq!(project["repo_path"], dir.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn create_with_a_relative_repo_path_is_400() {
        let server = TestServer::start().await;
        let response = server
            .post(
                "/projects",
                json!({ "name": "demo", "repo_path": "relative/path" }),
            )
            .await;
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn create_with_a_nonexistent_repo_path_is_400() {
        let server = TestServer::start().await;
        let response = server
            .post(
                "/projects",
                json!({ "name": "demo", "repo_path": "/definitely/does/not/exist/anywhere" }),
            )
            .await;
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn patch_with_null_repo_path_clears_it() {
        let server = TestServer::start().await;
        let dir = server.temp_dir();
        let project: Value = server
            .post(
                "/projects",
                json!({ "name": "demo", "repo_path": dir.to_string_lossy() }),
            )
            .await
            .json();
        let id = project["id"].as_str().unwrap().to_string();

        let patched: Value = server
            .patch(&format!("/projects/{id}"), json!({ "repo_path": null }))
            .await
            .json();
        assert!(patched["repo_path"].is_null(), "{patched}");
    }

    #[tokio::test]
    async fn patch_with_only_name_still_renames() {
        let server = TestServer::start().await;
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let id = project["id"].as_str().unwrap().to_string();

        let patched: Value = server
            .patch(&format!("/projects/{id}"), json!({ "name": "renamed" }))
            .await
            .json();
        assert_eq!(patched["name"], "renamed");
        assert!(patched["repo_path"].is_null());
    }

    #[tokio::test]
    async fn patch_with_neither_field_is_400() {
        let server = TestServer::start().await;
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let id = project["id"].as_str().unwrap().to_string();

        let response = server.patch(&format!("/projects/{id}"), json!({})).await;
        assert_eq!(response.status(), 400);
    }

    // ---- init-workflows (issue #88) ----

    #[tokio::test]
    async fn init_workflows_seeds_the_repo_and_reports_created_files() {
        let server = TestServer::start().await;
        let dir = server.temp_dir();
        let project: Value = server
            .post(
                "/projects",
                json!({ "name": "demo", "repo_path": dir.to_string_lossy() }),
            )
            .await
            .json();
        let id = project["id"].as_str().unwrap().to_string();

        let response = server
            .post(&format!("/projects/{id}/init-workflows"), json!({}))
            .await;
        assert_eq!(response.status(), 200, "body: {}", response.json());
        let body = response.json();
        assert!(
            body["dir"]
                .as_str()
                .unwrap()
                .ends_with(".chocofactory/workflows"),
            "{body}"
        );
        let created = body["created"].as_array().unwrap();
        assert!(
            created
                .iter()
                .any(|p| p.as_str().unwrap().ends_with("chat.yaml")),
            "{body}"
        );
        assert!(
            std::path::Path::new(&dir)
                .join(".chocofactory/workflows/chat.yaml")
                .is_file()
        );

        // A second call reports everything as already existing.
        let second = server
            .post(&format!("/projects/{id}/init-workflows"), json!({}))
            .await
            .json();
        assert!(second["created"].as_array().unwrap().is_empty());
        assert!(!second["existing"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn init_workflows_on_an_unknown_project_is_404() {
        let server = TestServer::start().await;
        let response = server
            .post("/projects/does-not-exist/init-workflows", json!({}))
            .await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn init_workflows_without_a_repo_path_is_409() {
        let server = TestServer::start().await;
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let id = project["id"].as_str().unwrap().to_string();

        let response = server
            .post(&format!("/projects/{id}/init-workflows"), json!({}))
            .await;
        assert_eq!(response.status(), 409);
    }

    // ---- max_agents ----

    #[tokio::test]
    async fn a_projects_agent_cap_round_trips_and_zero_removes_it() {
        let server = TestServer::start().await;
        let created: Value = server
            .post("/projects", json!({ "name": "capped", "max_agents": 2 }))
            .await
            .json();
        assert_eq!(created["max_agents"], 2);
        let id = created["id"].as_str().unwrap().to_string();
        let plain: Value = server
            .post("/projects", json!({ "name": "plain" }))
            .await
            .json();
        assert!(plain["max_agents"].is_null());

        let listed: Value = server.get("/projects").await.json();
        let caps: Vec<&Value> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|p| &p["max_agents"])
            .collect();
        assert!(
            caps.contains(&&json!(2)) && caps.contains(&&Value::Null),
            "{caps:?}"
        );

        // A PATCH with only the cap is accepted, and leaves the name alone.
        let raised: Value = server
            .patch(&format!("/projects/{id}"), json!({ "max_agents": 5 }))
            .await
            .json();
        assert_eq!(raised["max_agents"], 5);
        assert_eq!(raised["name"], "capped");
        // A PATCH without the key leaves it alone.
        let renamed: Value = server
            .patch(&format!("/projects/{id}"), json!({ "name": "renamed" }))
            .await
            .json();
        assert_eq!(renamed["max_agents"], 5);
        // 0 stores no cap, and so does null.
        let zero: Value = server
            .patch(&format!("/projects/{id}"), json!({ "max_agents": 0 }))
            .await
            .json();
        assert!(zero["max_agents"].is_null(), "{zero}");
        server
            .patch(&format!("/projects/{id}"), json!({ "max_agents": 3 }))
            .await;
        let null: Value = server
            .patch(&format!("/projects/{id}"), json!({ "max_agents": null }))
            .await
            .json();
        assert!(null["max_agents"].is_null(), "{null}");
        // Created with 0 means no cap too.
        let zero_create: Value = server
            .post("/projects", json!({ "name": "zero", "max_agents": 0 }))
            .await
            .json();
        assert!(zero_create["max_agents"].is_null());
    }

    #[tokio::test]
    async fn a_project_cap_that_is_not_a_non_negative_integer_is_rejected() {
        let server = TestServer::start().await;
        for bad in [json!(-1), json!("two"), json!(1.5)] {
            let response = server
                .post("/projects", json!({ "name": "p", "max_agents": bad }))
                .await;
            assert!(
                response.status() == 422 || response.status() == 400,
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_project_patch_is_rejected() {
        let server = TestServer::start().await;
        let project: Value = server
            .post("/projects", json!({ "name": "p" }))
            .await
            .json();
        let id = project["id"].as_str().unwrap();
        let response = server.patch(&format!("/projects/{id}"), json!({})).await;
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn raising_a_project_cap_wakes_the_dispatcher_at_once() {
        // The test server's dispatcher ticks once an hour, so only the wake
        // that `PATCH /projects/{id}` sends can start the queued turn.
        let server = TestServer::start().await;
        server.write_workflow(
            "one",
            "name: one\nroles:\n  r:\n    cli: claude\n    model: sonnet\nstages:\n  work:\n    kind: agent_turn\n    role: r\n    on: { done: finished }\n  finished:\n    kind: terminal\n",
        );
        let project: Value = server
            .post("/projects", json!({ "name": "capped", "max_agents": 1 }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap().to_string();
        let create = |title: &'static str| {
            let body = json!({
                "project_id": project_id, "workflow_def": "one", "title": title, "prompt": "go",
            });
            let server = &server;
            async move { server.post("/tasks", body).await.json() }
        };
        let first: Value = create("first").await;
        let second: Value = create("second").await;
        let second_id = second["id"].as_str().unwrap().to_string();
        let detail: Value = server.get(&format!("/tasks/{second_id}")).await.json();
        assert_eq!(detail["queued"][0]["position"], 1, "{detail}");
        assert!(first["id"].is_string());

        server
            .patch(
                &format!("/projects/{project_id}"),
                json!({ "max_agents": 2 }),
            )
            .await;
        crate::test_support::wait_until("the queued turn to start", || async {
            let detail: Value = server.get(&format!("/tasks/{second_id}")).await.json();
            if detail["queued"] == json!([]) {
                Ok(())
            } else {
                Err(detail["queued"].to_string())
            }
        })
        .await;
    }
}
