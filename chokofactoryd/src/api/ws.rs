//! Live events WebSocket (P1-9, design §6.1: task detail's live event
//! timeline). Pushes a task's backlog on connect, then live-tails new
//! events as they land — woken by `AppState::events_notify` rather than
//! polling on a timer (see `session.rs`'s `SessionManager::events_notify`
//! doc comment for why this is a single shared `Notify`, not a per-task
//! registry).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use chrono::{DateTime, Utc};

use super::AppState;
use crate::db::events;

pub async fn task_events(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, task_id))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, task_id: String) {
    let Ok(backlog) = events::list_for_task(&state.pool, &task_id).await else {
        return;
    };
    let mut cursor: Option<(DateTime<Utc>, String)> =
        backlog.last().map(|e| (e.created_at, e.id.clone()));
    for event in &backlog {
        if !send_event(&mut socket, event).await {
            return;
        }
    }

    loop {
        // Created *before* the query below so a `notify_waiters()` landing
        // between this line and the `.await` further down still resolves
        // it — `Notify` snapshots its notify-generation counter at
        // creation time specifically to make this ordering race-free, per
        // its own docs. Querying first (rather than always awaiting) means
        // a wakeup that arrives while this loop is still catching up on a
        // prior one isn't missed either.
        let notified = state.events_notify.notified();

        let Ok(new_events) =
            events::list_for_task_after(&state.pool, &task_id, cursor.as_ref()).await
        else {
            return;
        };

        if new_events.is_empty() {
            tokio::select! {
                biased;
                msg = socket.recv() => {
                    // `None` is a closed connection; `Some(Err(_))` a
                    // protocol error — both mean there's no one left to
                    // push to. Any `Some(Ok(_))` (a client ping/pong or
                    // stray message) is ignored — this is a push-only
                    // stream, not a chat input — and the loop just
                    // re-queries on the next iteration.
                    if !matches!(msg, Some(Ok(_))) {
                        return;
                    }
                }
                _ = notified => {}
            }
            continue;
        }

        for event in &new_events {
            if !send_event(&mut socket, event).await {
                return;
            }
        }
        cursor = new_events.last().map(|e| (e.created_at, e.id.clone()));
    }
}

async fn send_event(socket: &mut WebSocket, event: &chokofactory_core::models::Event) -> bool {
    let Ok(text) = serde_json::to_string(event) else {
        return false;
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    use super::super::tests::TestServer;

    async fn create_chat_task(server: &TestServer) -> String {
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
        task["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn connecting_replays_the_backlog_then_streams_new_events_live() {
        let server = TestServer::start().await;
        let task_id = create_chat_task(&server).await;

        // Give the initial turn a moment to produce its first events
        // (session_meta + the "hello" echo) before connecting, so the
        // backlog isn't empty.
        let mut saw_initial_echo = false;
        for _ in 0..200 {
            let events = crate::db::events::list_for_task(server.pool(), &task_id)
                .await
                .unwrap();
            if events
                .iter()
                .any(|e| e.payload.get("text").and_then(Value::as_str) == Some("echo:hello"))
            {
                saw_initial_echo = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(saw_initial_echo, "initial turn never produced its echo");

        let (mut ws, _) = connect_async(format!("{}/tasks/{task_id}/events", server.ws_url))
            .await
            .unwrap();

        // Backlog first: the very first message(s) received must already
        // contain the "hello" echo produced before this socket connected —
        // proves the connect-time bootstrap works, not just live delivery.
        let mut backlog_has_hello_echo = false;
        for _ in 0..10 {
            let Ok(Some(Ok(WsMessage::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(2), ws.next()).await
            else {
                break;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["payload"]["text"] == "echo:hello" {
                backlog_has_hello_echo = true;
                break;
            }
        }
        assert!(
            backlog_has_hello_echo,
            "backlog was not replayed on connect"
        );

        // Now prove live delivery is push-driven (the Notify wakeup path),
        // not something this test has to poll for: send a follow-up
        // message over HTTP, then read the *already-open* socket with a
        // bounded timeout and expect its echo to arrive without this test
        // ever touching the database again.
        let response = server
            .post(
                &format!("/tasks/{task_id}/messages"),
                json!({ "text": "again" }),
            )
            .await;
        assert_eq!(response.status(), 202);

        let mut saw_live_echo = false;
        for _ in 0..20 {
            let Ok(Some(Ok(WsMessage::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(5), ws.next()).await
            else {
                break;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["payload"]["text"] == "echo:again" {
                saw_live_echo = true;
                break;
            }
        }
        assert!(
            saw_live_echo,
            "live event was not pushed over the already-open socket"
        );

        let _ = ws.close(None).await;
    }

    #[tokio::test]
    async fn connecting_to_a_task_with_no_events_yet_sends_nothing_until_one_arrives() {
        let server = TestServer::start().await;

        // A task with no task_run/session at all (created directly at the
        // db layer, bypassing create_task) — a genuinely empty backlog,
        // unlike a real chat task which already has events by the time a
        // socket could connect.
        let project = crate::db::projects::create(server.pool(), "demo")
            .await
            .unwrap();
        let task = crate::db::tasks::create(
            server.pool(),
            crate::db::tasks::NewTask {
                project_id: &project.id,
                parent_task_id: None,
                workflow_def: "chat",
                title: "t",
                config: json!({}),
            },
        )
        .await
        .unwrap();

        let (mut ws, _) = connect_async(format!("{}/tasks/{}/events", server.ws_url, task.id))
            .await
            .unwrap();

        // Nothing to replay and nothing new yet — the socket should sit
        // quietly rather than error or close.
        let msg = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
        assert!(
            msg.is_err(),
            "socket produced a message with no events to send"
        );

        let _ = ws.close(None).await;
    }
}
