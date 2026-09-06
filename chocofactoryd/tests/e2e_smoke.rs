//! True e2e test for issue #42: spawns the *actual* `chocofactoryd`
//! binary (not the in-process `TestServer` router `api/mod.rs`'s other
//! tests use) as a subprocess, `mock-claude` standing in for `claude` via
//! `CHOCOFACTORY_CLAUDE_BINARY` so no real, billable CLI is spawned, and
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
        let path = std::env::temp_dir().join(format!("chocofactoryd-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        TempHome(path)
    }

    /// Pre-seeds a workflow definition before the daemon starts — safe
    /// because `seed_builtin_workflows` only ever writes `chat.yaml` when
    /// absent (`create_new`, never overwrites), so this is untouched by
    /// startup.
    fn write_workflow(&self, name: &str, yaml: &str) {
        let dir = self.0.join(".config/chocofactory/workflows");
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
/// colliding with a real `chocofactoryd` a developer might already have
/// running on the hardcoded default (4141).
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("failed to bind an ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

/// Drives the real `chocofactoryd` binary as a subprocess.
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
        Self::spawn_with_home_and_env(home, &[]).await
    }

    /// Like [`Self::spawn_with_home`], but with extra environment for the
    /// daemon. The adapter's subprocess inherits it, so this is how a test
    /// drives `mock-claude`'s behaviour (a fixed reply, one-shot exit, tool
    /// use) without a separate binary.
    async fn spawn_with_home_and_env(home: TempHome, env: &[(&str, &str)]) -> Self {
        let daemon_bin = workspace_binary("chocofactoryd");
        let mock_claude_bin = workspace_binary("mock-claude");
        assert!(
            daemon_bin.exists(),
            "chocofactoryd binary not found at {daemon_bin:?} \
             (run `cargo build --workspace --all-targets` first)"
        );
        assert!(
            mock_claude_bin.exists(),
            "mock-claude binary not found at {mock_claude_bin:?} \
             (run `cargo build --workspace --all-targets` first)"
        );

        let port = free_port();

        let mut command = Command::new(&daemon_bin);
        command
            .env("HOME", &home.0)
            .env("CHOCOFACTORY_CLAUDE_BINARY", &mock_claude_bin)
            .env("CHOCOFACTORY_PORT", port.to_string())
            .env("RUST_LOG", "error")
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("failed to spawn chocofactoryd");

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
            panic!("chocofactoryd exited during startup with {status:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("chocofactoryd did not become ready within 5s");
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

    // The premise this test rests on, up to this point: no session has
    // opened, so the only event recorded so far is the entry stage's own
    // transition. Resuming the gate below intentionally adds a
    // `human_message` event (#59) — checked explicitly further down —
    // this assertion just pins that nothing *else* has snuck in first.
    let events = history["events"].as_array().unwrap();
    assert!(
        events.iter().all(|e| e["event_type"] == "stage_entered"),
        "a human_gate-only task should have recorded nothing but its entry transition so far: {events:?}"
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

    // The human's message is recorded (#59) before the resume advances the
    // stage, so it's the first thing pushed live.
    let human_message = next_event(&mut ws)
        .await
        .expect("human message was not pushed over the already-open socket");
    assert_eq!(human_message["event_type"], "human_message");
    assert_eq!(human_message["payload"]["text"], "go");
    assert_eq!(human_message["task_id"], task_id.as_str());
    assert_eq!(human_message["task_run_id"], Value::Null);

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

/// The whole P2-3/#45 feature through the shipped binary: a reviewer turn
/// captures its reply as JSON, the graph routes on that reply's `outcome`
/// key, and a later stage templates a *different* field of the same capture
/// into its own command.
///
/// The in-process engine tests cover each half, but only this one proves the
/// wiring that exists solely in `main.rs` and the real adapter — and it runs
/// the mock with `MOCK_CLAUDE_TOOL_USE`, so the agent narrates and calls a
/// tool before answering. That is what every real turn touching a tool looks
/// like, and a capture that concatenated everything the agent said would fail
/// to parse here, fall back to `done`, and never reach `report` with a
/// verdict at all.
#[tokio::test]
async fn real_binary_routes_a_turn_on_its_captured_verdict_and_templates_it_onward() {
    let home = TempHome::new();
    home.write_workflow(
        "capture-e2e",
        r#"
name: capture-e2e
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on:
      approved: report
      changes_requested: report
  report:
    kind: shell
    command: "echo verdict={{ stages.review.comments }}"
    on: { done: done }
  done:
    kind: terminal
"#,
    );
    let daemon = Daemon::spawn_with_home_and_env(
        home,
        &[
            (
                "MOCK_CLAUDE_REPLY",
                r#"{"outcome": "approved", "comments": "ship-it"}"#,
            ),
            // Deliberately *not* MOCK_CLAUDE_ONESHOT: `mock-claude` without
            // it stays open on stdin after replying, exactly like the real
            // `claude --input-format stream-json` CLI (#70). A capturing
            // single-shot turn must conclude on its own `result` line, not
            // on the mock's test-only self-exit shortcut.
            ("MOCK_CLAUDE_TOOL_USE", "1"),
        ],
    )
    .await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "capture-e2e",
                "title": "capture smoke",
                "prompt": "review this",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // The task runs to completion only if the verdict routed and the
    // templated command rendered.
    for _ in 0..200 {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        if detail["workflow_state"]["current_stage"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(
        detail["workflow_state"]["current_stage"], "done",
        "task did not finish: {detail}"
    );

    // The capture landed under the stage that produced it, narration and
    // tool output excluded.
    let captured = &detail["workflow_state"]["payload"]["stages"]["review"];
    assert_eq!(captured["outcome"], "approved", "got {captured}");
    assert_eq!(captured["comments"], "ship-it", "got {captured}");

    // The reply's own verdict — not `done` — is what carried the transition.
    let trail: Vec<(&str, &str)> = detail["stage_trail"]
        .as_array()
        .expect("stage_trail missing")
        .iter()
        .map(|e| {
            (
                e["payload"]["stage"].as_str().unwrap(),
                e["payload"]["outcome"].as_str().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(
        trail,
        vec![("review", "-"), ("report", "approved"), ("done", "done")],
        "expected the captured verdict to route the graph"
    );

    // And a *different* field of that same capture reached the next stage's
    // command, rendered rather than left as a placeholder.
    let shell = command_events(&daemon, &task_id).await;
    assert_eq!(shell.len(), 1, "expected one shell_output: {shell:?}");
    assert_eq!(shell[0]["payload"]["command"], "echo verdict=ship-it");
    assert_eq!(shell[0]["payload"]["stdout_tail"], "verdict=ship-it");

    // The turn recorded what it did, and that it was applied.
    let turn: Vec<Value> = daemon.get(&format!("/tasks/{task_id}/events")).await["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == "turn_outcome")
        .cloned()
        .collect();
    assert_eq!(turn.len(), 1, "expected one turn_outcome: {turn:?}");
    assert_eq!(turn[0]["payload"]["outcome"], "approved");
    assert_eq!(turn[0]["payload"]["applied"], true);
    assert_eq!(turn[0]["payload"]["note"], Value::Null);
}

// ---- built-in coding-task workflow, real daemon (P2-7, issue #18) --------
//
// The engine-level tests in `engine.rs` already cover the loop-guard/
// revision-routing mechanics directly against the real workflow file; what
// this one proves instead is that the whole stack — real daemon process,
// HTTP API, startup seeding of `coding-task.yaml` and its prompts, and the
// worktree it opts into — actually delivers a task through it, same as the
// other `real_binary_*` tests do for chat/poll/capture.

struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "chocofactoryd-e2e-coding-task-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    TempDir(path)
}

async fn git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn write_script(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

#[tokio::test]
async fn real_binary_walks_the_coding_task_workflow_to_done() {
    let repo = tempdir();
    git(&repo, &["init", "-q"]).await;
    git(&repo, &["config", "user.email", "test@example.com"]).await;
    git(&repo, &["config", "user.name", "Test"]).await;
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]).await;
    git(&repo, &["commit", "-q", "-m", "init"]).await;
    let origin = tempdir();
    git(&origin, &["init", "-q", "--bare"]).await;
    git(
        &repo,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    )
    .await;

    // A stub `gh` covering exactly the three invocations
    // `coding-task.yaml` makes, backed by real `git`/the real repo above
    // for everything else. Scoped to just this daemon subprocess's `PATH`
    // (passed via `spawn_with_home_and_env`'s `env`) — unlike the
    // equivalent `engine.rs` test, this test controls the `Command` that
    // spawns the daemon directly, so there's no need to mutate the test
    // process's own environment to get a stub in scope.
    let scripts_dir = tempdir();
    write_script(
        &scripts_dir,
        "gh",
        &format!(
            r#"#!/bin/sh
set -eu
created="{dir}/pr-created"
case "$1" in
    api)
        # `awaiting_human_review` makes two `gh api` calls: the head
        # commit's date, then the comment list its filter runs over. The
        # stub answers the second from a file the test owns — which means
        # it stands in for the *whole* query, jq filter included. That
        # filter is covered separately and directly by
        # `verdict_filter_*` in `tests/verdict_filter.rs`; what these
        # workflow tests cover is the routing either side of it.
        if printf '%s\n' "$@" | grep -q '/comments'; then
            cat "{dir}/verdict" 2>/dev/null || true
        else
            echo "2020-01-01T00:00:00Z"
        fi
        ;;
    pr)
        case "$2" in
            create)
                echo created >> "$created"
                echo "https://example.test/pr/42"
                ;;
            list)
                # `open_pr`'s probe and its read-back, both scoped to open
                # PRs. Empty until `pr create` has run, so the first lap
                # creates and every later lap reuses.
                if [ -s "$created" ]; then
                    if printf '%s\n' "$@" | grep -q url; then
                        echo '{{"number": 42, "url": "https://example.test/pr/42"}}'
                    else
                        echo 42
                    fi
                fi
                ;;
            view)
                echo "0000000000000000000000000000000000000000"
                ;;
            checks)
                echo "SUCCESS"
                ;;
            *)
                echo "stub gh: unhandled pr subcommand: $*" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "stub gh: unhandled subcommand: $*" >&2
        exit 1
        ;;
esac
"#,
            dir = scripts_dir.to_string_lossy(),
        ),
    );
    let path_with_stub = format!(
        "{}:{}",
        scripts_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A single fake `claude` binary standing in for both `coder` and
    // `reviewer` — `CHOCOFACTORY_CLAUDE_BINARY` is one binary for the
    // whole daemon, same limitation `engine.rs`'s equivalent helper's doc
    // comment explains. `adapter/claude.rs::spawn` passes `--system-prompt
    // <text>` whenever a role resolves one, and `coder-system.md`/
    // `reviewer-system.md` open with distinct wording — the wrapper greps
    // its own argv for that marker.
    //
    // Deliberately does not set MOCK_CLAUDE_ONESHOT: every stage this
    // workflow walks (`coding`, `internal_review`, `revising`) is a
    // single-shot `agent_turn`, and `mock-claude` without that flag stays
    // open on stdin after replying — exactly the real `claude
    // --input-format stream-json` CLI's shape (#70). This is the whole
    // multi-role workflow proving that shape doesn't wedge it.
    let mock_claude = workspace_binary("mock-claude");
    assert!(
        mock_claude.exists(),
        "mock-claude binary not found at {mock_claude:?} \
         (run `cargo build --workspace --all-targets` first)"
    );
    std::fs::write(
        scripts_dir.join("reviewer-reply.json"),
        r#"{"outcome": "approved", "summary": ""}"#,
    )
    .unwrap();
    // The token `awaiting_human_review`'s filter emits for an approval;
    // the stub stands in for the query that would derive it.
    std::fs::write(scripts_dir.join("verdict"), "APPROVE\n").unwrap();
    let claude_wrapper = write_script(
        &scripts_dir,
        "mock-claude-role-dispatch.sh",
        &format!(
            r#"#!/bin/sh
set -eu
role="coder"
for arg in "$@"; do
    case "$arg" in
        *"reviewing agent"*) role="reviewer" ;;
    esac
done
if [ "$role" = "reviewer" ]; then
    export MOCK_CLAUDE_REPLY="$(cat "{scripts_dir}/reviewer-reply.json")"
else
    export MOCK_CLAUDE_REPLY="did the thing"
fi
exec "{mock_claude}" "$@"
"#,
            scripts_dir = scripts_dir.to_string_lossy(),
            mock_claude = mock_claude.display(),
        ),
    );

    let home = TempHome::new();
    let daemon = Daemon::spawn_with_home_and_env(
        home,
        &[
            (
                "CHOCOFACTORY_CLAUDE_BINARY",
                &claude_wrapper.to_string_lossy(),
            ),
            ("PATH", &path_with_stub),
        ],
    )
    .await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "coding-task",
                "title": "Add a small feature",
                "prompt": "Add a small feature",
                "config": { "cwd": repo.to_string_lossy() },
            }),
        )
        .await;
    assert_eq!(status, 201, "task creation failed: {task}");
    let task_id = task["id"].as_str().unwrap().to_string();

    for _ in 0..300 {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        if detail["workflow_state"]["current_stage"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(
        detail["workflow_state"]["current_stage"], "done",
        "task did not finish: {detail}"
    );
    assert_eq!(detail["status"], "closed");

    let trail: Vec<&str> = detail["stage_trail"]
        .as_array()
        .expect("stage_trail missing")
        .iter()
        .map(|e| e["payload"]["stage"].as_str().unwrap())
        .collect();
    assert_eq!(
        trail,
        vec![
            "coding",
            "internal_review",
            "open_pr",
            "checks_polling",
            "awaiting_human_review",
            "done",
        ]
    );
}

/// Cancel end to end through the real daemon binary (#69).
///
/// The in-process API tests already cover the status transitions; what
/// only a real spawned daemon shows is the whole path working together —
/// a genuinely live `mock-claude` subprocess, killed by a real signal,
/// with the task left readable afterwards.
#[tokio::test]
async fn real_binary_cancels_a_live_task_and_refuses_further_work() {
    let daemon = Daemon::spawn().await;

    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let project_id = project["id"].as_str().unwrap();

    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project_id,
                "workflow_def": "chat",
                "title": "cancel me",
                "prompt": "hello",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // Wait until the session is genuinely live before cancelling — a
    // cancel that raced the spawn would prove nothing about teardown.
    let (mut ws, _) = connect_async(format!("{}/tasks/{task_id}/events/live", daemon.ws_url))
        .await
        .expect("failed to open the events websocket");
    assert!(
        wait_for_echo(&mut ws, "echo:hello").await,
        "did not see the initial turn's echoed reply over the live WS"
    );

    let (status, _) = daemon
        .post(&format!("/tasks/{task_id}/cancel"), json!({}))
        .await;
    assert_eq!(status, 202);

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["status"], "cancelled");
    // Still readable, and still says where it stopped — the difference
    // between cancelling a task and deleting it.
    assert_eq!(detail["workflow_state"]["current_stage"], "chatting");

    // A second cancel conflicts rather than silently succeeding.
    let (status, _) = daemon
        .post(&format!("/tasks/{task_id}/cancel"), json!({}))
        .await;
    assert_eq!(status, 409);

    // And the task takes no further work: without the `tasks.status`
    // guard this would be accepted and would resume a fresh subprocess
    // from the persisted session_id.
    let (status, _) = daemon
        .post(
            &format!("/tasks/{task_id}/messages"),
            json!({ "text": "still there?" }),
        )
        .await;
    assert_eq!(status, 409);
}
