//! chokofactoryd's HTTP/WS API layer (P1-9, design §6.1, §6.2): project
//! CRUD, task create/list/status, send-message, and a live events
//! WebSocket. No auth — bound to `127.0.0.1` only by whoever calls
//! [`router`] (Q15).

mod error;
mod events;
mod projects;
mod tasks;
mod ws;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use sqlx::SqlitePool;
use tokio::sync::Notify;

use crate::engine::WorkflowEngine;

pub use error::ApiError;

/// Shared state every handler gets via `State<AppState>`. Cheap to clone
/// (a pool handle, two `Arc`s) — axum clones it per request.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub engine: Arc<WorkflowEngine>,
    pub events_notify: Arc<Notify>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/projects", post(projects::create).get(projects::list))
        .route(
            "/projects/{id}",
            get(projects::get)
                .patch(projects::rename)
                .delete(projects::delete),
        )
        .route("/tasks", post(tasks::create).get(tasks::list))
        .route("/tasks/{id}", get(tasks::get).patch(tasks::update_config))
        .route("/tasks/{id}/messages", post(tasks::send_message))
        .route("/tasks/{id}/events", get(events::list))
        .route("/tasks/{id}/events/live", get(ws::task_events))
        .with_state(state)
}

/// Shared HTTP-level test harness (P1-9): every `api/*.rs` module's tests
/// drive a real axum server bound to an ephemeral `127.0.0.1` port over
/// `db::connect_in_memory()`, matching the rest of this codebase's
/// preference (`session.rs`, `engine.rs`) for exercising real behavior —
/// real subprocess, real timing, and here, a real bound socket — over
/// mocks.
#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::Value;
    use sqlx::SqlitePool;
    use tokio::sync::Notify;

    use super::{AppState, router};
    use crate::adapter::{AgentAdapter, ClaudeAdapter};
    use crate::db;
    use crate::engine::WorkflowEngine;
    use crate::session::SessionManager;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("chokofactoryd-api-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const CHAT_WORKFLOW_YAML: &str = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    pub struct TestResponse {
        status: u16,
        body: Vec<u8>,
    }

    impl TestResponse {
        pub fn status(&self) -> u16 {
            self.status
        }

        pub fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }
    }

    pub struct TestServer {
        base_url: String,
        pub ws_url: String,
        client: reqwest::Client,
        workflows_dir: TempDir,
        pool: SqlitePool,
    }

    impl TestServer {
        /// Boots a real server with the `fake_claude.py` fixture (already
        /// used by `session.rs`/`engine.rs`'s tests — a persistent echo
        /// script) wired in as the `claude` binary, so a test creating a
        /// `chat` task exercises a real (if fake) live session end to end.
        pub async fn start() -> Self {
            Self::start_with_adapter_binary("fake_claude.py").await
        }

        /// Like [`Self::start`], but with a different fixture binary — e.g.
        /// `fake_claude_oneshot.py`, which exits cleanly right away, for
        /// tests that need a single-shot `agent_turn` to actually complete
        /// and auto-advance.
        pub async fn start_with_adapter_binary(binary: &str) -> Self {
            let pool = db::connect_in_memory().await.unwrap();
            let workflows_dir = TempDir::new();
            let events_notify = Arc::new(Notify::new());
            let adapter: Arc<dyn AgentAdapter> =
                Arc::new(ClaudeAdapter::with_binary(fixture_binary(binary)));
            let session_manager = SessionManager::new(
                pool.clone(),
                adapter,
                chrono::Duration::hours(1),
                Arc::clone(&events_notify),
            );
            let engine = WorkflowEngine::new(
                pool.clone(),
                session_manager,
                workflows_dir.0.clone(),
                None,
                Arc::clone(&events_notify),
            );
            let state = AppState {
                pool: pool.clone(),
                engine,
                events_notify,
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, router(state)).await;
            });
            TestServer {
                base_url: format!("http://{addr}"),
                ws_url: format!("ws://{addr}"),
                client: reqwest::Client::new(),
                workflows_dir,
                pool,
            }
        }

        pub fn pool(&self) -> &SqlitePool {
            &self.pool
        }

        pub fn seed_chat_workflow(&self) {
            std::fs::write(self.workflows_dir.0.join("chat.yaml"), CHAT_WORKFLOW_YAML).unwrap();
        }

        pub fn write_workflow(&self, name: &str, yaml: &str) {
            std::fs::write(self.workflows_dir.0.join(format!("{name}.yaml")), yaml).unwrap();
        }

        async fn to_response(resp: reqwest::Response) -> TestResponse {
            let status = resp.status().as_u16();
            let body = resp.bytes().await.unwrap().to_vec();
            TestResponse { status, body }
        }

        pub async fn get(&self, path: &str) -> TestResponse {
            let resp = self
                .client
                .get(format!("{}{path}", self.base_url))
                .send()
                .await
                .unwrap();
            Self::to_response(resp).await
        }

        pub async fn post(&self, path: &str, body: Value) -> TestResponse {
            let resp = self
                .client
                .post(format!("{}{path}", self.base_url))
                .json(&body)
                .send()
                .await
                .unwrap();
            Self::to_response(resp).await
        }

        pub async fn patch(&self, path: &str, body: Value) -> TestResponse {
            let resp = self
                .client
                .patch(format!("{}{path}", self.base_url))
                .json(&body)
                .send()
                .await
                .unwrap();
            Self::to_response(resp).await
        }

        pub async fn delete(&self, path: &str) -> TestResponse {
            let resp = self
                .client
                .delete(format!("{}{path}", self.base_url))
                .send()
                .await
                .unwrap();
            Self::to_response(resp).await
        }
    }
}
