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

    /// Pre-seeds a workflow definition before the daemon starts — safe
    /// because `seed_builtin_workflows` only ever writes `chat.yaml` when
    /// absent (`create_new`, never overwrites), so this is untouched by
    /// startup.
    fn write_workflow(&self, name: &str, yaml: &str) {
        let dir = self.0.join(".config/chokofactory/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.yaml")), yaml).unwrap();
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
        Self::spawn_with_home(TempHome::new()).await
    }

    /// Like [`Self::spawn`], but against a home directory the caller has
    /// already seeded (e.g. with [`TempHome::write_workflow`]) — the
    /// workflows dir is read at startup, so it has to be populated before
    /// the process exists.
    async fn spawn_with_home(home: TempHome) -> Self {
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

/// Reads the next WS frame as a parsed event, or `None` on timeout.
async fn next_event(
    ws: &mut (
             impl futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
) -> Option<Value> {
    let Ok(Some(Ok(WsMessage::Text(raw)))) =
        tokio::time::timeout(Duration::from_secs(5), ws.next()).await
    else {
        return None;
    };
    serde_json::from_str(&raw).ok()
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

/// X-3's headline behavior, against the real binary: a stage transition is
/// an event on the task's timeline and reaches a live subscriber.
///
/// Every stage here is a `human_gate`, so the task opens no agent session
/// and produces *zero* conversation events for its whole life — the
/// transitions are the only thing there is to stream. That makes this the
/// case nothing else can cover by accident: before X-3 a subscriber to this
/// task would have seen nothing at all, ever, because `events.task_run_id`
/// was `NOT NULL` and no run exists to attribute a transition to.
///
/// `api/ws.rs` asserts the same thing in-process. This one goes through the
/// spawned daemon, so it also proves the wiring that only exists in
/// `main.rs`: the single `Arc<Notify>` shared between `SessionManager` and
/// `WorkflowEngine`. Hand the engine its own `Notify` there and the
/// in-process test still passes while the shipped daemon goes silent.
#[tokio::test]
async fn real_binary_pushes_a_stage_transition_over_ws_with_no_session_involved() {
    let home = TempHome::new();
    home.write_workflow(
        "gated-e2e",
        r#"
name: gated-e2e
stages:
  gate:
    kind: human_gate
    on: { resumed: review }
  review:
    kind: human_gate
    on: { approved: done }
  done:
    kind: terminal
"#,
    );
    let daemon = Daemon::spawn_with_home(home).await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "gated-e2e",
                "title": "gated smoke",
                "prompt": "start",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // Let the entry stage's transition land before connecting, so the
    // backlog is settled rather than racing the socket. Polled over HTTP —
    // unlike the in-process test there's no pool to look at from here.
    let mut history = Value::Null;
    let mut recorded = false;
    for _ in 0..100 {
        history = daemon.get(&format!("/tasks/{task_id}/events")).await;
        if !history["events"].as_array().unwrap().is_empty() {
            recorded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(recorded, "entry stage was never recorded: {history}");

    // The premise this test rests on: this workflow really is silent apart
    // from its transitions. If a conversation event ever showed up here,
    // the WS assertions below could pass for the wrong reason.
    let events = history["events"].as_array().unwrap();
    assert!(
        events.iter().all(|e| e["event_type"] == "stage_entered"),
        "a human_gate-only task should produce nothing but transitions: {events:?}"
    );

    let (mut ws, _) = connect_async(format!("{}/tasks/{task_id}/events/live", daemon.ws_url))
        .await
        .expect("failed to open the events websocket");

    // Replayed backlog: the stage the task started in.
    let backlog = next_event(&mut ws)
        .await
        .expect("entry stage transition was not replayed on connect");
    assert_eq!(backlog["event_type"], "stage_entered");
    assert_eq!(backlog["payload"]["stage"], "gate");
    assert_eq!(backlog["payload"]["outcome"], Value::Null);
    assert_eq!(backlog["task_id"], task_id.as_str());
    assert_eq!(backlog["task_run_id"], Value::Null);

    // Resume the gate. This advances the workflow without starting any
    // session, so the engine's own notify for the transition it just
    // recorded is the only thing that can wake this socket.
    let (status, _) = daemon
        .post(
            &format!("/tasks/{task_id}/messages"),
            json!({ "text": "go" }),
        )
        .await;
    assert_eq!(status, 202);

    let live = next_event(&mut ws)
        .await
        .expect("stage transition was not pushed over the already-open socket");
    assert_eq!(live["event_type"], "stage_entered");
    assert_eq!(live["payload"]["stage"], "review");
    assert_eq!(live["payload"]["outcome"], "resumed");
    assert_eq!(live["task_id"], task_id.as_str());
    assert_eq!(live["task_run_id"], Value::Null);

    // The same transition is served by the real binary's `GET /tasks/:id`
    // as `stage_trail`, so the live and polled views agree.
    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["workflow_state"]["current_stage"], "review");
    let trail: Vec<&str> = detail["stage_trail"]
        .as_array()
        .expect("stage_trail missing from the real binary's task detail")
        .iter()
        .map(|e| e["payload"]["stage"].as_str().unwrap())
        .collect();
    assert_eq!(trail, vec!["gate", "review"]);

    let _ = ws.close(None).await;
}

/// Every `shell_output` entry on a task, oldest first.
async fn command_events(daemon: &Daemon, task_id: &str) -> Vec<Value> {
    daemon.get(&format!("/tasks/{task_id}/events")).await["events"]
        .as_array()
        .expect("events endpoint returned no array")
        .iter()
        .filter(|e| e["event_type"] == "shell_output")
        .cloned()
        .collect()
}

/// Reads WS frames until a `stage_entered` for `stage` shows up.
async fn wait_for_stage_entered(
    ws: &mut (
             impl futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
    stage: &str,
) -> Option<Value> {
    // A poll pushes its own command output over the same socket, so the
    // transition is not necessarily the next frame.
    for _ in 0..20 {
        let event = next_event(ws).await?;
        if event["event_type"] == "stage_entered" && event["payload"]["stage"] == stage {
            return Some(event);
        }
    }
    None
}

/// The `poll` stage kind (P2-2) driven through the real binary: the
/// in-process engine tests exercise `WorkflowEngine` directly, so only this
/// shows that a *detached* poll runner survives inside the actual daemon
/// and that what it records reaches a client over real HTTP and WS.
///
/// Costs nothing to run: a poll stage opens no agent session, so unlike the
/// chat test above this never invokes `mock-claude` at all.
///
/// Scoped to the match path deliberately. The timeout, spawn-failure and
/// capture paths are covered in-process, and a second daemon spin-up here
/// would add seconds of wall time for no extra integration coverage.
#[tokio::test]
async fn real_binary_runs_a_poll_stage_until_its_command_output_changes() {
    let home = TempHome::new();
    // The state the poll watches. An absolute path inside the temp home
    // (cleaned up with it) so the workflow needs no `cwd` on the task.
    let marker = home.0.join("checks-state");
    std::fs::write(&marker, "PENDING\n").unwrap();

    home.write_workflow(
        "poll-e2e",
        &format!(
            r#"
name: poll-e2e
stages:
  polling:
    kind: poll
    command: "cat {}"
    interval: 1s
    timeout: 60s
    outcomes:
      - match: "SUCCESS"
        then: green
    on: {{ green: done, timeout: stalled }}
  done:
    kind: terminal
  stalled:
    kind: human_gate
    on: {{ resumed: done }}
"#,
            marker.display()
        ),
    );
    // Deliberately generous against the 1s interval: this test asserts the
    // *match* path, and a slow CI box must not flake into `stalled`. The
    // `timeout` edge exists so that if it ever does, the failure names the
    // stage it got stuck in instead of just timing out.
    let daemon = Daemon::spawn_with_home(home).await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "poll-e2e",
                "title": "poll smoke",
                "prompt": "start",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // Wait for the poll to actually report an attempt, so what follows is
    // testing a running loop rather than racing its startup.
    let mut pending = Vec::new();
    for _ in 0..200 {
        pending = command_events(&daemon, &task_id).await;
        if !pending.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        pending.len(),
        1,
        "expected exactly one progress entry while the state was unchanged: {pending:?}"
    );
    assert_eq!(pending[0]["payload"]["stdout_tail"], "PENDING");
    assert_eq!(pending[0]["payload"]["attempt"], 1);
    // A poll stage opens no session, so its output belongs to the task
    // itself and carries no run id.
    assert_eq!(pending[0]["task_run_id"], Value::Null);
    assert_eq!(pending[0]["task_id"], task_id.as_str());

    // Opened before the state flips, so the transition below can only
    // arrive by being pushed rather than replayed from the backlog.
    let (mut ws, _) = connect_async(format!("{}/tasks/{task_id}/events/live", daemon.ws_url))
        .await
        .expect("failed to open the events websocket");

    // Written-then-renamed so a `cat` racing this can't read a half-written
    // file and match on nothing.
    let staged = marker.with_extension("next");
    std::fs::write(&staged, "SUCCESS\n").unwrap();
    std::fs::rename(&staged, &marker).unwrap();

    let live = wait_for_stage_entered(&mut ws, "done")
        .await
        .expect("the poll's transition was never pushed over the socket");
    assert_eq!(live["payload"]["outcome"], "green");
    assert_eq!(live["task_run_id"], Value::Null);

    let events = command_events(&daemon, &task_id).await;
    assert_eq!(
        events.len(),
        2,
        "expected one progress entry and one decisive entry: {events:?}"
    );
    let decisive = &events[1]["payload"];
    assert_eq!(decisive["outcome"], "green");
    assert_eq!(decisive["matched"], "SUCCESS");
    assert_eq!(decisive["stdout_tail"], "SUCCESS");
    assert!(
        decisive["note"].as_str().unwrap().contains("matched"),
        "the decisive entry should say which rule fired: {decisive}"
    );

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["workflow_state"]["current_stage"], "done");
    assert_eq!(detail["status"], "closed");
    let trail: Vec<&str> = detail["stage_trail"]
        .as_array()
        .expect("stage_trail missing from the real binary's task detail")
        .iter()
        .map(|e| e["payload"]["stage"].as_str().unwrap())
        .collect();
    assert_eq!(trail, vec!["polling", "done"]);

    let _ = ws.close(None).await;
}
