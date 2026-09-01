//! Project CRUD handlers (P1-9, design §6.2: `choco project create`/`list`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chocofactory_core::models::Project;
use serde::Deserialize;

use super::{ApiError, AppState};
use crate::db::{projects, tasks};

#[derive(Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<Project>), ApiError> {
    let project = projects::create(&state.pool, &body.name).await?;
    Ok((StatusCode::CREATED, Json(project)))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<Project>>, ApiError> {
    let projects = projects::list(&state.pool).await?;
    Ok(Json(projects))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Project>, ApiError> {
    let project = projects::get(&state.pool, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such project '{id}'")))?;
    Ok(Json(project))
}

#[derive(Deserialize)]
pub struct RenameProjectRequest {
    pub name: String,
}

pub async fn rename(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameProjectRequest>,
) -> Result<Json<Project>, ApiError> {
    let project = projects::rename(&state.pool, &id, &body.name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no such project '{id}'")))?;
    Ok(Json(project))
}

/// 409 if the project still has tasks, rather than letting the delete hit
/// `tasks.project_id`'s foreign key (`db::pool::connect` enables
/// `foreign_keys`, and there's no `ON DELETE CASCADE` on that reference) —
/// a pre-check here reports the actual reason in a stable, backend-agnostic
/// shape instead of depending on how sqlx surfaces a FK violation.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
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
}
