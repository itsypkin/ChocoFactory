//! Paginated events history (`GET /tasks/:id/events`, distinct from the
//! live WS stream at `/tasks/:id/events/live`) — lets a client that
//! missed events (disconnected socket, or one that never wants to hold a
//! live connection at all) catch up in bounded pages instead of loading a
//! task's entire history in one response.

use axum::Json;
use axum::extract::{Path, Query, State};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chocofactory_core::models::Event;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ApiError, AppState};
use crate::db::{events, tasks};

/// Default/max page size for `limit` — unset defaults to
/// [`DEFAULT_LIMIT`]; anything above [`MAX_LIMIT`] is silently capped
/// rather than rejected, so a client that always passes a huge number
/// doesn't need special-case error handling.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Deserialize)]
pub struct ListEventsQuery {
    /// Opaque token from a previous page's `next_token` — omitted (or
    /// `None`) means "from the beginning."
    pub after: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// `Some` iff there may be more events after this page — pass it back
    /// as `after` to fetch the next page. `None` means this page reached
    /// the end of the task's history as of this call.
    pub next_token: Option<String>,
}

/// Encodes a `(created_at, id)` cursor as an opaque token — callers should
/// treat this as a black box, not parse it, so the internal cursor shape
/// (currently mirroring `db::events`'s own `(DateTime<Utc>, String)`
/// pagination cursor) is free to change later without breaking clients.
fn encode_cursor(created_at: DateTime<Utc>, id: &str) -> String {
    let raw = format!("{}\0{id}", created_at.to_rfc3339());
    URL_SAFE_NO_PAD.encode(raw)
}

fn decode_cursor(token: &str) -> Result<(DateTime<Utc>, String), ApiError> {
    let invalid = || ApiError::BadRequest(format!("invalid 'after' token '{token}'"));
    let raw = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
    let raw = String::from_utf8(raw).map_err(|_| invalid())?;
    let (created_at, id) = raw.split_once('\0').ok_or_else(invalid)?;
    let created_at = DateTime::parse_from_rfc3339(created_at)
        .map_err(|_| invalid())?
        .with_timezone(&Utc);
    Ok((created_at, id.to_string()))
}

/// Checks `task_id` exists before querying (same reasoning as the WS
/// handler's own check, P1-9 review): an empty page is otherwise
/// indistinguishable from "no such task."
pub async fn list(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
    Query(query): Query<ListEventsQuery>,
) -> Result<Json<EventsPage>, ApiError> {
    if tasks::get(&state.pool, &task_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("no such task '{task_id}'")));
    }

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let cursor = query.after.as_deref().map(decode_cursor).transpose()?;

    // Fetch one extra row: if it comes back, there's a next page, and its
    // presence (not its content) is all that's needed to know that — it's
    // dropped below, never returned to the client.
    let mut page =
        events::list_for_task_page(&state.pool, &task_id, cursor.as_ref(), limit as i64 + 1)
            .await?;

    let next_token = if page.len() > limit {
        page.truncate(limit);
        page.last().map(|e| encode_cursor(e.created_at, &e.id))
    } else {
        None
    };

    Ok(Json(EventsPage {
        events: page,
        next_token,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::tests::TestServer;

    #[tokio::test]
    async fn paginates_through_a_tasks_full_event_history() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap();
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

        // Wait for the initial turn's events (session_meta + echo) so
        // there's a real, non-trivial history to page through.
        let mut total = 0usize;
        for _ in 0..200 {
            let events = crate::db::events::list_for_task(server.pool(), &task_id)
                .await
                .unwrap();
            if events.len() >= 2 {
                total = events.len();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(total >= 2, "expected at least 2 events to page through");

        // Page through with limit=1, following next_token, and confirm we
        // see exactly the same events in the same order as an unpaginated
        // fetch, with no duplicates or gaps.
        let mut paged_ids = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let path = match &after {
                Some(token) => format!("/tasks/{task_id}/events?limit=1&after={token}"),
                None => format!("/tasks/{task_id}/events?limit=1"),
            };
            let page: Value = server.get(&path).await.json();
            let events = page["events"].as_array().unwrap();
            assert!(events.len() <= 1);
            for event in events {
                paged_ids.push(event["id"].as_str().unwrap().to_string());
            }
            after = page["next_token"].as_str().map(|s| s.to_string());
            if after.is_none() {
                break;
            }
        }

        let unpaginated: Value = server.get(&format!("/tasks/{task_id}/events")).await.json();
        let unpaginated_ids: Vec<String> = unpaginated["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paged_ids, unpaginated_ids);
        assert_eq!(unpaginated["next_token"], Value::Null);
    }

    #[tokio::test]
    async fn stage_transitions_interleave_with_conversation_events_in_the_history() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap();
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

        // Wait for the initial turn's reply to land on top of the entry
        // stage's transition. Waits on that specific event rather than on a
        // count, so a slow fixture subprocess under a loaded test run
        // doesn't leave a partial history to assert against.
        let mut events = Vec::new();
        let mut saw_reply = false;
        for _ in 0..500 {
            let history: Value = server.get(&format!("/tasks/{task_id}/events")).await.json();
            events = history["events"].as_array().unwrap().clone();
            if events
                .iter()
                .any(|e| e["event_type"] == "assistant_message")
            {
                saw_reply = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let seen: Vec<&str> = events
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
            .collect();
        assert!(saw_reply, "initial turn never produced a reply: {seen:?}");

        // The stage transition comes first: `start_task` records it before
        // the session it opens can emit anything.
        assert_eq!(events[0]["event_type"], "stage_entered");
        assert_eq!(events[0]["payload"]["stage"], "chatting");
        assert_eq!(events[0]["payload"]["outcome"], Value::Null);
        // It belongs to the task, not to a session — and is served over the
        // same endpoint as everything else rather than a parallel one.
        assert_eq!(events[0]["task_run_id"], Value::Null);
        assert_eq!(events[0]["task_id"], task_id.as_str());

        // Everything after it is session-scoped conversation, in order.
        let rest: Vec<&str> = events[1..]
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
            .collect();
        assert!(
            rest.iter().all(|t| *t != "stage_entered"),
            "chat has one stage, so only one transition should appear: {rest:?}"
        );
        assert!(
            events[1..].iter().all(|e| e["task_run_id"] != Value::Null),
            "conversation events should still name their session"
        );
        assert!(
            rest.contains(&"assistant_message"),
            "the reply must sort after the transition, not before it: {rest:?}"
        );
    }

    #[tokio::test]
    async fn the_human_side_of_the_conversation_shows_up_in_the_paginated_history() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap();
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

        server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "again" }),
            )
            .await;

        let mut page = Value::Null;
        for _ in 0..200 {
            page = server.get(&format!("/tasks/{task_id}/events")).await.json();
            let texts: Vec<&str> = page["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event_type"] == "human_message")
                .map(|e| e["payload"]["text"].as_str().unwrap())
                .collect();
            if texts == vec!["hello", "again"] {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("human_message events never showed up as expected: {page}");
    }

    #[tokio::test]
    async fn events_for_a_nonexistent_task_is_404() {
        let server = TestServer::start().await;
        let status = server.get("/tasks/does-not-exist/events").await.status();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn an_invalid_after_token_is_400() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap();
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

        let status = server
            .get(&format!("/tasks/{task_id}/events?after=not-a-real-token"))
            .await
            .status();
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn limit_is_capped_rather_than_rejected() {
        let server = TestServer::start().await;
        server.seed_chat_workflow();
        let project: Value = server
            .post("/projects", json!({ "name": "demo" }))
            .await
            .json();
        let project_id = project["id"].as_str().unwrap();
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

        let response = server
            .get(&format!("/tasks/{task_id}/events?limit=999999999"))
            .await;
        assert_eq!(response.status(), 200);
    }
}
