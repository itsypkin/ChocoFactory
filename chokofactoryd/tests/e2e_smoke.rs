//! True e2e test for issue #42: spawns the *actual* `chokofactoryd`
//! binary (not the in-process `TestServer` router `api/mod.rs`'s other
//! tests use) as a subprocess, `mock-claude` standing in for `claude` via
//! `CHOKOFACTORY_CLAUDE_BINARY` so no real, billable CLI is spawned, and
//! drives it over real HTTP + WS. Covers the full startup sequence
//! (migrate, seed builtin workflows, recover stale runs, idle reaper,
//! retention) that `TestServer` skips by construction.

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("chokofactoryd-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        TempHome(path)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Locates a sibling workspace binary next to this test binary
/// (`target/<profile>/deps/<test-exe>` -> `target/<profile>/<name>`)
/// rather than via Cargo artifact/bindep dependencies, which this repo's
/// stable toolchain doesn't use. Relies on the workspace's bin targets
/// having already been built (this repo's verification gate always runs
/// `cargo build --workspace --all-targets` before `cargo test
/// --workspace`; plain `cargo test --workspace` also builds every
/// member's own bin target as a matter of course).
fn workspace_binary(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("test binary has no path");
    path.pop(); // strip the test binary's own filename
    if path.ends_with("deps") {
        path.pop();
    }
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    path.join(exe_name)
}

/// Binds an ephemeral port and immediately releases it, so the daemon
/// subprocess (started moments later) can bind it instead — avoids
/// colliding with a real `chokofactoryd` a developer might already have
/// running on the hardcoded default (4141).
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("failed to bind an ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

/// Drives the real `chokofactoryd` binary as a subprocess.
struct Daemon {
    child: Child,
    base_url: String,
    ws_url: String,
    client: reqwest::Client,
    _home: TempHome,
}

impl Daemon {
    async fn spawn() -> Self {
        let daemon_bin = workspace_binary("chokofactoryd");
        let mock_claude_bin = workspace_binary("mock-claude");
        assert!(
            daemon_bin.exists(),
            "chokofactoryd binary not found at {daemon_bin:?} \
             (run `cargo build --workspace --all-targets` first)"
        );
        assert!(
            mock_claude_bin.exists(),
            "mock-claude binary not found at {mock_claude_bin:?} \
             (run `cargo build --workspace --all-targets` first)"
        );

        let home = TempHome::new();
        let port = free_port();

        let mut child = Command::new(&daemon_bin)
            .env("HOME", &home.0)
            .env("CHOKOFACTORY_CLAUDE_BINARY", &mock_claude_bin)
            .env("CHOKOFACTORY_PORT", port.to_string())
            .env("RUST_LOG", "error")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn chokofactoryd");

        let base_url = format!("http://127.0.0.1:{port}");
        let ws_url = format!("ws://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        wait_until_ready(&client, &base_url, &mut child).await;

        Daemon {
            child,
            base_url,
            ws_url,
            client,
            _home: home,
        }
    }

    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .client
            .post(format!("{}{path}", self.base_url))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // `kill_on_drop(true)` also covers this, but `try_wait`/`kill`
        // here happen synchronously rather than on tokio's next poll.
        let _ = self.child.start_kill();
    }
}

async fn wait_until_ready(client: &reqwest::Client, base_url: &str, child: &mut Child) {
    for _ in 0..100 {
        if let Ok(resp) = client.get(format!("{base_url}/projects")).send().await
            && resp.status().is_success()
        {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("chokofactoryd exited during startup with {status:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("chokofactoryd did not become ready within 5s");
}

/// Reads WS frames until an `assistant_message` event carrying `text`
/// shows up, or gives up.
async fn wait_for_echo(
    ws: &mut (
             impl futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
    text: &str,
) -> bool {
    for _ in 0..10 {
        let Ok(Some(Ok(WsMessage::Text(raw)))) =
            tokio::time::timeout(Duration::from_secs(2), ws.next()).await
        else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if event["event_type"] == "assistant_message" && event["payload"]["text"] == text {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn real_binary_serves_a_chat_task_end_to_end_over_http_and_ws() {
    let daemon = Daemon::spawn().await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    // "chat" isn't written by this test — it's the real daemon's own
    // `seed_builtin_workflows` startup step (§2.2) putting it on disk,
    // which `TestServer`'s in-process tests always write by hand instead.
    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "chat",
                "title": "smoke",
                "prompt": "hello",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["workflow_state"]["current_stage"], "chatting");

    let (mut ws, _) = connect_async(format!("{}/tasks/{task_id}/events/live", daemon.ws_url))
        .await
        .expect("failed to open the events websocket");

    assert!(
        wait_for_echo(&mut ws, "echo:hello").await,
        "did not see the initial turn's echoed reply over the live WS"
    );

    // A follow-up message on the same still-open agent_turn proves the
    // live send path works end to end through a real spawned daemon, not
    // just the in-process router `TestServer` exercises elsewhere.
    let (status, _) = daemon
        .post(
            &format!("/tasks/{task_id}/messages"),
            json!({ "text": "again" }),
        )
        .await;
    assert_eq!(status, 202);

    assert!(
        wait_for_echo(&mut ws, "echo:again").await,
        "did not see the follow-up message's echoed reply over the live WS"
    );
}
