//! True e2e test for issue #42: spawns the *actual* `chocofactoryd`
//! binary (not the in-process `TestServer` router `api/mod.rs`'s other
//! tests use) as a subprocess, `mock-claude` standing in for `claude` via
//! `CHOCOFACTORY_CLAUDE_BINARY` so no real, billable CLI is spawned, and
//! drives it over real HTTP + WS. Covers the full startup sequence
//! (migrate, seed builtin workflows, recover stale runs, idle reaper,
//! retention) that `TestServer` skips by construction.

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// One source of truth for the load-tolerant wait budgets (#98, #152): the
// same file the unit tests use. `RESPONSE_MARGIN` is unused here.
#[path = "../src/test_support.rs"]
#[allow(dead_code)]
mod test_support;
use test_support::{LOAD_ALLOWANCE, wait_until};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("chocofactoryd-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        TempHome(path)
    }

    /// Writes a workflow definition to `<home>/test-workflows/<name>.yaml`
    /// and returns its path, to create a task with `"workflow_file"` (#129:
    /// the daemon no longer reads a global workflows folder).
    fn write_workflow(&self, name: &str, yaml: &str) -> PathBuf {
        let dir = self.0.join("test-workflows");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
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
    /// `Some` until [`Self::kill`] hands it back.
    home: Option<TempHome>,
    port: u16,
}

/// Bounds the `free_port`-race retry (see `Daemon::spawn_with_home_and_env`)
/// so a run that keeps losing doesn't loop forever.
const MAX_SPAWN_ATTEMPTS: u32 = 5;

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

        let client = reqwest::Client::new();

        // `free_port` can only *suggest* a port: it releases the port before
        // the daemon binds it, so between those two moments another test in
        // this suite (they run in parallel), another worktree's `cargo test`,
        // or a developer's own daemon can take it, and this one exits on the
        // failed bind. That's retried on a fresh port rather than propagated,
        // because a lost race says nothing about the code under test.
        //
        // Only a confirmed bind failure is retried, though: a blanket retry
        // on any startup exit would silently paper over a real crash (e.g. a
        // broken migration), so the child's stderr is piped and checked for
        // the daemon's own bind-failure message before looping.
        let mut last_failure = None;
        for _ in 0..MAX_SPAWN_ATTEMPTS {
            let port = free_port();

            let mut command = Command::new(&daemon_bin);
            command
                .env("HOME", &home.0)
                .env("CHOCOFACTORY_CLAUDE_BINARY", &mock_claude_bin)
                .env("CHOCOFACTORY_PORT", port.to_string())
                .env("RUST_LOG", "error")
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (key, value) in env {
                command.env(key, value);
            }
            let mut child = command.spawn().expect("failed to spawn chocofactoryd");

            let base_url = format!("http://127.0.0.1:{port}");
            let ws_url = format!("ws://127.0.0.1:{port}");

            match wait_until_ready(&client, &base_url, &mut child).await {
                Ready::Yes => {
                    // Stderr was piped (not inherited) so a failed startup
                    // could be inspected for the bind-failure message above;
                    // now that startup succeeded, drain it for the rest of
                    // the daemon's life so its output still reaches the test
                    // log (as it would if inherited) and so an undrained
                    // pipe can never fill and deadlock the daemon.
                    spawn_stderr_forwarder(&mut child);
                    return Daemon {
                        child,
                        base_url,
                        ws_url,
                        client,
                        home: Some(home),
                        port,
                    };
                }
                // Only an early exit is retried, and only when it's actually
                // a lost port race. A daemon that started but never answered
                // is a real failure, and `wait_until_ready` panics on it
                // rather than returning.
                Ready::ExitedDuringStartup(status) => {
                    let stderr = read_stderr_to_string(&mut child).await;
                    if !stderr_says_bind_failed(&stderr) {
                        panic!("chocofactoryd exited during startup with {status:?}: {stderr}");
                    }
                    last_failure = Some((status, stderr));
                }
            }
        }
        let (status, stderr) = last_failure.expect("loop above runs at least once");
        panic!(
            "chocofactoryd lost the free_port race {MAX_SPAWN_ATTEMPTS} times in a row; \
             last exit {status:?}: {stderr}"
        );
    }

    /// SIGKILLs the daemon (no graceful shutdown, like a crash) and hands
    /// back its home and port so a second daemon can take over the same state.
    async fn kill(mut self) -> (TempHome, u16) {
        self.child
            .kill()
            .await
            .expect("failed to SIGKILL chocofactoryd");
        let home = self.home.take().expect("home already taken");
        (home, self.port)
    }

    /// Starts a daemon on an explicit `port` and an existing `home` — the
    /// restart half of a kill-and-restart test. One attempt, no port-race
    /// retry: the port was just released by the daemon that was killed.
    async fn restart_on(home: TempHome, port: u16) -> Self {
        Self::restart_on_with_env(home, port, &[]).await
    }

    async fn restart_on_with_env(home: TempHome, port: u16, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(workspace_binary("chocofactoryd"));
        command
            .env("HOME", &home.0)
            .env(
                "CHOCOFACTORY_CLAUDE_BINARY",
                workspace_binary("mock-claude"),
            )
            .env("CHOCOFACTORY_PORT", port.to_string())
            .env("RUST_LOG", "error")
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("failed to spawn chocofactoryd");
        let client = reqwest::Client::new();
        let base_url = format!("http://127.0.0.1:{port}");
        match wait_until_ready(&client, &base_url, &mut child).await {
            Ready::Yes => {
                spawn_stderr_forwarder(&mut child);
                Daemon {
                    child,
                    base_url,
                    ws_url: format!("ws://127.0.0.1:{port}"),
                    client,
                    home: Some(home),
                    port,
                }
            }
            Ready::ExitedDuringStartup(status) => {
                let stderr = read_stderr_to_string(&mut child).await;
                panic!("restarted chocofactoryd exited during startup with {status:?}: {stderr}");
            }
        }
    }

    /// SIGTERMs the daemon and waits for it to exit on its own, handing back
    /// its home and port, like [`Self::kill`] but through the graceful path.
    async fn terminate(mut self) -> (std::process::ExitStatus, TempHome, u16) {
        let status = sigterm_and_wait(&mut self.child).await;
        let home = self.home.take().expect("home already taken");
        (status, home, self.port)
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

/// Outcome of waiting for the daemon's first successful response.
enum Ready {
    Yes,
    ExitedDuringStartup(std::process::ExitStatus),
}

async fn wait_until_ready(client: &reqwest::Client, base_url: &str, child: &mut Child) -> Ready {
    // A wall-clock deadline plus a per-probe timeout, so a daemon that
    // accepts the connection but never answers (e.g. blocked writing to a
    // full stderr pipe) still reaches the kill-and-report path below
    // instead of hanging the test.
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .get(format!("{base_url}/projects"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            && resp.status().is_success()
        {
            return Ready::Yes;
        }
        // Reported back so the caller can retry on a fresh port (a lost
        // `free_port` race looks exactly like this) instead of failing the
        // test outright.
        if let Ok(Some(status)) = child.try_wait() {
            return Ready::ExitedDuringStartup(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Checked once more so an exit during the final sleep is still
    // reported as one (and retried if it was a lost race), not as a hang.
    if let Ok(Some(status)) = child.try_wait() {
        return Ready::ExitedDuringStartup(status);
    }
    // Not retried: a hang is a genuine bug, not a lost port race. Killed
    // first so its stderr pipe reaches EOF and the panic carries whatever the
    // daemon said — including when it hung because that undrained pipe filled.
    if let Err(err) = child.start_kill() {
        eprintln!("failed to kill unresponsive chocofactoryd: {err}");
    }
    let stderr = read_stderr_to_string(child).await;
    panic!("chocofactoryd did not become ready within 5s: {stderr}");
}

/// Forwards a live daemon's stderr, line by line, to the test process's own
/// stderr (`cargo test` captures and prints that per-test on failure), and
/// keeps the pipe drained so the daemon can never block on a full pipe
/// buffer. Only called once the daemon is confirmed up, so `child.stderr`
/// is still `Some` here.
fn spawn_stderr_forwarder(child: &mut Child) {
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => eprintln!("[chocofactoryd] {line}"),
                    Ok(None) => break,
                    // Said out loud rather than treated as EOF: breaking
                    // closes the read end, so the daemon's further stderr
                    // writes fail and its output from here on is lost.
                    Err(err) => {
                        eprintln!("[chocofactoryd] stopped forwarding stderr: read failed: {err}");
                        break;
                    }
                }
            }
        });
    }
}

/// Reads an exited (or just-killed) child's piped stderr to EOF. Bounded,
/// since anything else still holding the pipe's write end would otherwise
/// keep it open indefinitely; whatever was read before the deadline is
/// still returned, and a read error or timeout is noted in the output
/// rather than dropped.
async fn read_stderr_to_string(child: &mut Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), stderr.read_to_end(&mut buf)).await;
    let mut out = String::from_utf8_lossy(&buf).into_owned();
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => out.push_str(&format!("\n[failed to read chocofactoryd stderr: {err}]")),
        Err(_) => {
            out.push_str("\n[chocofactoryd stderr still open after 2s; output may be truncated]")
        }
    }
    out
}

/// Distinguishes a lost `free_port` race (safe to retry on a fresh port)
/// from any other startup failure (a real bug that must surface, not be
/// retried into silence). Matches the daemon's own `.expect` message at
/// `chocofactoryd/src/main.rs`'s `TcpListener::bind` call.
fn stderr_says_bind_failed(stderr: &str) -> bool {
    stderr.contains("chocofactoryd: failed to bind 127.0.0.1")
}

/// Reads the next WS frame as a parsed event, or `None` on timeout.
async fn next_event(
    ws: &mut (
             impl futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
) -> Option<Value> {
    let Ok(Some(Ok(WsMessage::Text(raw)))) = tokio::time::timeout(LOAD_ALLOWANCE, ws.next()).await
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
/// task would have seen nothing at all, ever, because `events.session_id`
/// was `NOT NULL` and no session exists to attribute a transition to.
///
/// `api/ws.rs` asserts the same thing in-process. This one goes through the
/// spawned daemon, so it also proves the wiring that only exists in
/// `main.rs`: the single `Arc<Notify>` shared between `SessionManager` and
/// `WorkflowEngine`. Hand the engine its own `Notify` there and the
/// in-process test still passes while the shipped daemon goes silent.
#[tokio::test]
async fn real_binary_pushes_a_stage_transition_over_ws_with_no_session_involved() {
    let home = TempHome::new();
    let gated_wf = home.write_workflow(
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
                "workflow_file": gated_wf,
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
    let history = wait_until("the entry stage to be recorded", || async {
        let history = daemon.get(&format!("/tasks/{task_id}/events")).await;
        if history["events"].as_array().unwrap().is_empty() {
            Err(format!("no events: {history}"))
        } else {
            Ok(history)
        }
    })
    .await;

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
    assert_eq!(backlog["session_id"], Value::Null);

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
    assert_eq!(human_message["session_id"], Value::Null);

    let live = next_event(&mut ws)
        .await
        .expect("stage transition was not pushed over the already-open socket");
    assert_eq!(live["event_type"], "stage_entered");
    assert_eq!(live["payload"]["stage"], "review");
    assert_eq!(live["payload"]["outcome"], "resumed");
    assert_eq!(live["task_id"], task_id.as_str());
    assert_eq!(live["session_id"], Value::Null);

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

    let poll_wf = home.write_workflow(
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
                "workflow_file": poll_wf,
                "title": "poll smoke",
                "prompt": "start",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // Wait for the poll to actually report an attempt, so what follows is
    // testing a running loop rather than racing its startup.
    let pending = wait_until("the poll to report an attempt", || async {
        let pending = command_events(&daemon, &task_id).await;
        if pending.is_empty() {
            Err("no command events yet".to_string())
        } else {
            Ok(pending)
        }
    })
    .await;
    assert_eq!(
        pending.len(),
        1,
        "expected exactly one progress entry while the state was unchanged: {pending:?}"
    );
    assert_eq!(pending[0]["payload"]["stdout_tail"], "PENDING");
    assert_eq!(pending[0]["payload"]["attempt"], 1);
    // A poll stage opens no session, so its output belongs to the task
    // itself and carries no session id.
    assert_eq!(pending[0]["session_id"], Value::Null);
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
    assert_eq!(live["session_id"], Value::Null);

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

    // `stage_entered done` is pushed before the status write that closes
    // the task, so wait for the close rather than racing it.
    let detail = wait_until("the task to be closed", || async {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        if detail["workflow_state"]["current_stage"] == "done" && detail["status"] == "closed" {
            Ok(detail)
        } else {
            Err(format!("task not yet closed: {detail}"))
        }
    })
    .await;
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
/// captures its verdict as JSON, the graph routes on its `outcome` key, and a
/// later stage templates a *different* field of the same capture into its own
/// command. Since #90 the verdict arrives through the `report_outcome` call
/// every single-shot turn makes to complete (#73's tool), not the reply text.
///
/// The in-process engine tests cover each half, but only this one proves the
/// wiring that exists solely in `main.rs` and the real adapter — and it runs
/// the mock with `MOCK_CLAUDE_TOOL_USE`, so the agent narrates and calls a
/// tool before answering, which is what every real turn touching a tool looks
/// like.
#[tokio::test]
async fn real_binary_routes_a_turn_on_its_captured_verdict_and_templates_it_onward() {
    let home = TempHome::new();
    let capture_wf = home.write_workflow(
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
            // The verdict goes through `report_outcome`, the way every
            // single-shot turn has to complete since #90, and a `capture:
            // json` stage routes on and captures that call's input.
            (
                "MOCK_CLAUDE_REPORT",
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
                "workflow_file": capture_wf,
                "title": "capture smoke",
                "prompt": "review this",
            }),
        )
        .await;
    assert_eq!(status, 201);
    let task_id = task["id"].as_str().unwrap().to_string();

    // The task runs to completion only if the verdict routed and the
    // templated command rendered.
    let detail = wait_until("the task to reach `done`", || async {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        // `current_stage` and `status` are committed in separate writes, so
        // wait for both rather than racing the second.
        if detail["workflow_state"]["current_stage"] == "done" && detail["status"] == "closed" {
            Ok(detail)
        } else {
            Err(format!("task did not finish and close: {detail}"))
        }
    })
    .await;
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
        # `awaiting_human_review` runs `scripts/await-review.sh`, which
        # makes the head commit's date call and then the comments calls.
        # The stub answers the comments calls like gh would: it applies the
        # call's `-q` filter (with `jq`) to a canned page the test owns in
        # the `verdict` file (a JSON array of PR comments). What a comment
        # has to look like to vote is covered in
        # `tests/await_review_script.rs`; these workflow tests cover the
        # routing either side of it.
        if printf '%s\n' "$@" | grep -q '/comments'; then
            q=""; prev=""
            for a in "$@"; do
                if [ "$prev" = "-q" ]; then q=$a; fi
                prev=$a
            done
            jq -r "$q" < "{dir}/verdict"
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
    // The PR comments `awaiting_human_review`'s script reads (the stub
    // applies the script's `-q` filter to this page): an owner's approval.
    std::fs::write(
        scripts_dir.join("verdict"),
        r#"[{"created_at": "2030-01-01T00:00:00Z", "updated_at": "2030-01-01T00:00:00Z", "author_association": "OWNER", "user": {"login": "owner"}, "html_url": "https://example.test/c/1", "body": "looks good\n/approve"}]"#,
    )
    .unwrap();
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
    export MOCK_CLAUDE_REPORT="$(cat "{scripts_dir}/reviewer-reply.json")"
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

    let detail = wait_until("the task to reach `done`", || async {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        // `current_stage` and `status` are committed in separate writes, so
        // wait for both rather than racing the second.
        if detail["workflow_state"]["current_stage"] == "done" && detail["status"] == "closed" {
            Ok(detail)
        } else {
            Err(format!("task did not finish and close: {detail}"))
        }
    })
    .await;
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

/// A poll-entry workflow that never matches, so it only ever ends by its
/// wall-clock `timeout`.
fn write_never_matching_poll(home: &TempHome, timeout: &str) -> PathBuf {
    home.write_workflow(
        "poll-restart",
        &format!(
            r#"
name: poll-restart
stages:
  polling:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: {timeout}
    outcomes:
      - match: "NEVER_MATCHES_XYZ"
        then: green
    on: {{ green: done, timeout: stalled }}
  done:
    kind: terminal
  stalled:
    kind: human_gate
    on: {{ resumed: done }}
"#
        ),
    )
}

async fn create_poll_restart_task(daemon: &Daemon, workflow_file: &std::path::Path) -> String {
    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project["id"].as_str().unwrap(),
                "workflow_file": workflow_file,
                "title": "restart smoke",
                "prompt": "start",
            }),
        )
        .await;
    assert_eq!(status, 201);
    task["id"].as_str().unwrap().to_string()
}

async fn stage_outcomes(daemon: &Daemon, task_id: &str) -> Vec<(String, Value)> {
    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    detail["stage_trail"]
        .as_array()
        .expect("stage_trail missing")
        .iter()
        .map(|e| {
            (
                e["payload"]["stage"].as_str().unwrap().to_string(),
                e["payload"]["outcome"].clone(),
            )
        })
        .collect()
}

/// #52 through the real binary: SIGKILL mid-poll, restart on the same
/// `$HOME`, and the poll resumes with the budget it had — it times out at
/// the original entry + 12s, not a fresh 12s after the restart.
#[tokio::test]
async fn real_binary_resumes_a_poll_after_a_kill_on_its_original_deadline() {
    let home = TempHome::new();
    let poll_wf = write_never_matching_poll(&home, "12s");
    let daemon = Daemon::spawn_with_home(home).await;
    let entered = std::time::Instant::now();
    let task_id = create_poll_restart_task(&daemon, &poll_wf).await;

    tokio::time::sleep(Duration::from_secs(8)).await;
    let (home, port) = daemon.kill().await;
    let daemon = Daemon::restart_on(home, port).await;

    // Original entry + 12s + 4s of slack for startup and the interval.
    let limit = entered + Duration::from_secs(16);
    let mut stage = String::new();
    while std::time::Instant::now() < limit {
        let detail = daemon.get(&format!("/tasks/{task_id}")).await;
        stage = detail["workflow_state"]["current_stage"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if stage == "stalled" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        stage, "stalled",
        "the resumed poll missed its original deadline"
    );
    let trail = stage_outcomes(&daemon, &task_id).await;
    assert!(
        trail.iter().any(|(s, o)| s == "polling" && o == "restart"),
        "expected a restart stage_entered: {trail:?}"
    );
}

/// The sweep runs only after the lock: a second daemon on the same `$HOME`
/// and port dies at the lock (before it even tries to bind), before it can
/// resume (and so duplicate) anything.
#[tokio::test]
async fn a_second_daemon_dies_at_the_lock_before_sweeping_polls() {
    let home = TempHome::new();
    let poll_wf = write_never_matching_poll(&home, "60s");
    let daemon = Daemon::spawn_with_home(home).await;
    let task_id = create_poll_restart_task(&daemon, &poll_wf).await;

    let home_path = daemon.home.as_ref().unwrap().0.clone();
    let mut second = spawn_raw(
        &home_path,
        &[("CHOCOFACTORY_PORT", &daemon.port.to_string())],
    );
    let status = tokio::time::timeout(LOAD_ALLOWANCE, second.wait())
        .await
        .expect("the second daemon did not exit")
        .unwrap();
    assert!(!status.success());
    let stderr = read_stderr_to_string(&mut second).await;
    assert!(
        stderr.contains("already running"),
        "unexpected stderr: {stderr}"
    );

    let trail = stage_outcomes(&daemon, &task_id).await;
    assert!(
        !trail.iter().any(|(_, o)| o == "restart"),
        "the losing daemon must not have resumed the poll: {trail:?}"
    );
}

// ---- #84: lock, graceful shutdown, park sweep, version ----

/// A daemon straight from the binary, with the default mock `claude` and
/// whatever `env` adds (later entries win). stderr piped; the caller owns it.
fn spawn_raw(home: &std::path::Path, env: &[(&str, &str)]) -> Child {
    let mut command = Command::new(workspace_binary("chocofactoryd"));
    command
        .env("HOME", home)
        .env(
            "CHOCOFACTORY_CLAUDE_BINARY",
            workspace_binary("mock-claude"),
        )
        .env("CHOCOFACTORY_PORT", free_port().to_string())
        .env("RUST_LOG", "error")
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    command.spawn().expect("failed to spawn chocofactoryd")
}

fn config_root_of(home: &TempHome) -> PathBuf {
    home.0.join(".config/chocofactory")
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only reports whether the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// SIGKILLs these pids on drop, so a failed test doesn't leave the fixture's
/// orphans running.
struct PidGuard(Vec<u32>);

impl Drop for PidGuard {
    fn drop(&mut self) {
        for pid in &self.0 {
            // SAFETY: a plain signal to a pid this test recorded.
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

async fn sigterm_and_wait(child: &mut Child) -> std::process::ExitStatus {
    let pid = child.id().expect("daemon already exited");
    // SAFETY: SIGTERM to the daemon this test spawned.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
    tokio::time::timeout(LOAD_ALLOWANCE, child.wait())
        .await
        .expect("the daemon did not exit within LOAD_ALLOWANCE of SIGTERM")
        .unwrap()
}

async fn read_pid_file(path: &std::path::Path) -> u32 {
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_session_meta(daemon: &Daemon, task_id: &str) {
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        let page = daemon.get(&format!("/tasks/{task_id}/events")).await;
        if page["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e["event_type"] == "session_meta"))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no session_meta event: {page}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn lock_is_released(home: &TempHome) -> bool {
    matches!(
        chocofactory_core::daemon_lock::read_lock(&config_root_of(home)).unwrap(),
        chocofactory_core::daemon_lock::LockState::NotRunning { .. }
    )
}

const AGENT_TURN_WORKFLOW: &str = r#"
name: agent-e2e
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;

fn agent_reason(stage: &str) -> String {
    format!(
        "stage '{stage}' was running an agent turn when the daemon stopped; 'choco task retry' \
         continues it, resuming the agent's session when it can"
    )
}

/// Everything a test needs to drive the spawns-child fixture as `claude`.
struct AgentFixture {
    heartbeat: PathBuf,
    child_pid: PathBuf,
    agent_pid: PathBuf,
}

impl AgentFixture {
    fn new(home: &TempHome) -> Self {
        AgentFixture {
            heartbeat: home.0.join("heartbeat"),
            child_pid: home.0.join("child.pid"),
            agent_pid: home.0.join("agent.pid"),
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "CHOCOFACTORY_CLAUDE_BINARY",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/fake_claude_spawns_child.py"
                )
                .to_string(),
            ),
            ("CHOCO_TEST_HEARTBEAT", self.heartbeat.display().to_string()),
            ("CHOCO_TEST_CHILD_PID", self.child_pid.display().to_string()),
            ("CHOCO_TEST_AGENT_PID", self.agent_pid.display().to_string()),
        ]
    }
}

async fn spawn_agent_daemon(home: TempHome, fixture: &AgentFixture) -> Daemon {
    let env = fixture.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Daemon::spawn_with_home_and_env(home, &env).await
}

async fn create_task(daemon: &Daemon, workflow_file: &std::path::Path) -> String {
    let (status, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    assert_eq!(status, 201);
    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project["id"].as_str().unwrap(),
                "workflow_file": workflow_file,
                "title": "lifecycle smoke",
                "prompt": "go",
            }),
        )
        .await;
    assert_eq!(status, 201, "{task}");
    task["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_second_daemon_on_the_same_home_and_another_port_is_refused() {
    let home = TempHome::new();
    let poll_wf = write_never_matching_poll(&home, "60s");
    let daemon = Daemon::spawn_with_home(home).await;
    let task_id = create_poll_restart_task(&daemon, &poll_wf).await;
    let before = stage_outcomes(&daemon, &task_id).await;

    // State the second daemon must not touch before it is refused: an
    // `active` session (session recovery would flip it) and a deleted
    // built-in (materializing would recreate it).
    let home_path = daemon.home.as_ref().unwrap().0.clone();
    let chat_yaml = home_path.join(".config/chocofactory/.builtin-workflows/chat.yaml");
    std::fs::remove_file(&chat_yaml).expect("the first daemon materializes chat.yaml");
    let db_url = format!(
        "sqlite://{}",
        home_path
            .join(".config/chocofactory/chocofactory.db")
            .display()
    );
    let pool = sqlx::SqlitePool::connect(&db_url).await.unwrap();
    sqlx::query(
        "INSERT INTO sessions (id, task_id, stage, role, cli_adapter, model, status, started_at) \
         VALUES ('planted-active', ?, 'x', 'r', 'claude', 'm', 'active', '2026-01-01T00:00:00Z')",
    )
    .bind(&task_id)
    .execute(&pool)
    .await
    .unwrap();

    let other_port = free_port();
    assert_ne!(other_port, daemon.port);
    let mut second = spawn_raw(
        &home_path,
        &[
            ("CHOCOFACTORY_PORT", &other_port.to_string()),
            ("RUST_LOG", "info"),
        ],
    );
    let status = tokio::time::timeout(LOAD_ALLOWANCE, second.wait())
        .await
        .expect("the second daemon did not exit")
        .unwrap();
    assert!(!status.success());
    let stderr = read_stderr_to_string(&mut second).await;
    assert!(stderr.contains("already running"), "{stderr}");
    let first_pid = daemon.child.id().unwrap();
    assert!(stderr.contains(&format!("pid {first_pid}")), "{stderr}");
    for forbidden in [
        "built-in workflows ready",
        "connected to database",
        "recovered stale active sessions",
    ] {
        assert!(!stderr.contains(forbidden), "{forbidden}: {stderr}");
    }

    assert_eq!(stage_outcomes(&daemon, &task_id).await, before);
    assert!(
        !chat_yaml.exists(),
        "the refused daemon rewrote the built-ins"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM sessions WHERE id = 'planted-active'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "active", "the refused daemon ran session recovery");
}

#[tokio::test]
async fn sigterm_mid_agent_turn_stops_the_agent_and_parks_the_task_for_retry() {
    let home = TempHome::new();
    let agent_wf = home.write_workflow("agent-e2e", AGENT_TURN_WORKFLOW);
    let fixture = AgentFixture::new(&home);
    let daemon = spawn_agent_daemon(home, &fixture).await;
    let task_id = create_task(&daemon, &agent_wf).await;

    let agent_pid = read_pid_file(&fixture.agent_pid).await;
    let child_pid = read_pid_file(&fixture.child_pid).await;
    let _guard = PidGuard(vec![agent_pid, child_pid]);
    wait_for_session_meta(&daemon, &task_id).await;

    let (status, home, port) = daemon.terminate().await;
    assert!(status.success(), "{status:?}");
    assert!(!pid_alive(agent_pid), "the agent outlived the daemon");
    assert!(
        !pid_alive(child_pid),
        "the agent's child outlived the daemon"
    );
    assert!(lock_is_released(&home));

    let daemon = Daemon::restart_on(home, port).await;
    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["status"], "stuck", "{detail}");
    assert_eq!(detail["stuck_reason"], agent_reason("coding").as_str());
    let (status, body) = daemon
        .post(&format!("/tasks/{task_id}/retry"), json!({}))
        .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["resumed"], true, "{body}");
}

#[tokio::test]
async fn sigterm_mid_shell_stage_kills_the_command_and_parks_the_task() {
    let home = TempHome::new();
    let gc = home.0.join("gc");
    let shell_wf = home.write_workflow(
        "shell-e2e",
        &format!(
            r#"
name: shell-e2e
stages:
  run:
    kind: shell
    command: "sh -c 'sleep 600 & echo $! > {}; wait'"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
            gc.display()
        ),
    );
    let daemon = Daemon::spawn_with_home(home).await;
    let task_id = create_task(&daemon, &shell_wf).await;
    let grandchild = read_pid_file(&gc).await;
    let _guard = PidGuard(vec![grandchild]);
    assert!(pid_alive(grandchild));

    let (status, home, port) = daemon.terminate().await;
    assert!(status.success(), "{status:?}");
    // The command's group is SIGKILLed as the runtime drops; allow the
    // kernel a moment to reap the orphan.
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    while pid_alive(grandchild) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !pid_alive(grandchild),
        "the shell command's child outlived the daemon"
    );

    let daemon = Daemon::restart_on(home, port).await;
    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["status"], "stuck", "{detail}");
    assert_eq!(
        detail["stuck_reason"],
        "stage 'run' was running a shell command when the daemon stopped; 'choco task retry' \
         runs it again from the start"
    );
}

/// The crash path: SIGKILL leaves the agent orphaned and the session row
/// `active`; the next startup recovers it to `idle` and the park sweep
/// records `daemon_stopped`.
#[tokio::test]
async fn sigkill_mid_agent_turn_is_parked_at_the_next_start() {
    let home = TempHome::new();
    let agent_wf = home.write_workflow("agent-e2e", AGENT_TURN_WORKFLOW);
    let fixture = AgentFixture::new(&home);
    let daemon = spawn_agent_daemon(home, &fixture).await;
    let task_id = create_task(&daemon, &agent_wf).await;
    let agent_pid = read_pid_file(&fixture.agent_pid).await;
    let child_pid = read_pid_file(&fixture.child_pid).await;
    // Killed by the test itself, since the daemon never got to.
    let _guard = PidGuard(vec![agent_pid, child_pid]);
    wait_for_session_meta(&daemon, &task_id).await;

    let (home, port) = daemon.kill().await;
    assert!(
        lock_is_released(&home),
        "SIGKILL must still release the lock"
    );
    let daemon = Daemon::restart_on(home, port).await;

    let detail = daemon.get(&format!("/tasks/{task_id}")).await;
    assert_eq!(detail["status"], "stuck", "{detail}");
    assert_eq!(detail["stuck_reason"], agent_reason("coding").as_str());
    let home_path = daemon.home.as_ref().unwrap().0.clone();
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}?mode=ro",
        home_path
            .join(".config/chocofactory/chocofactory.db")
            .display()
    ))
    .await
    .unwrap();
    let reasons: Vec<Option<String>> =
        sqlx::query_scalar("SELECT end_reason FROM sessions WHERE task_id = ?")
            .bind(&task_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(reasons, vec![Some("daemon_stopped".to_string())]);
}

#[tokio::test]
async fn port_zero_binds_a_free_port_and_publishes_it_in_the_lock_file() {
    let home = TempHome::new();
    let mut child = spawn_raw(&home.0, &[("CHOCOFACTORY_PORT", "0")]);
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    let info = loop {
        if let Ok(chocofactory_core::daemon_lock::LockState::Running(info)) =
            chocofactory_core::daemon_lock::read_lock(&config_root_of(&home))
            && info.port != 0
        {
            break info;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the daemon exited: {}",
            read_stderr_to_string(&mut child).await
        );
        assert!(tokio::time::Instant::now() < deadline, "no lock info");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(info.pid, child.id().unwrap());
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/server", info.port);
    let answer_deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    let body = loop {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            break resp.json::<Value>().await.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < answer_deadline,
            "GET /server never answered on the lock file's port"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(body["port"], info.port);
    assert_eq!(body["pid"], info.pid);
    assert_eq!(body["version"], chocofactory_core::version::VERSION);
    sigterm_and_wait(&mut child).await;
}

#[tokio::test]
async fn version_flag_prints_and_touches_nothing() {
    let home = TempHome::new();
    let out = Command::new(workspace_binary("chocofactoryd"))
        .arg("--version")
        .env("HOME", &home.0)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "chocofactoryd {} (dev build)\n",
            chocofactory_core::version::VERSION
        )
    );
    assert!(!home.0.join(".config").exists());
}

/// A request that never finishes must not hold shutdown hostage: the HTTP
/// drain deadline gives up on it and the daemon still exits 0 and releases
/// the lock.
#[tokio::test]
async fn a_stalled_connection_does_not_block_shutdown_past_the_drain_grace() {
    use tokio::io::AsyncWriteExt;

    let home = TempHome::new();
    let mut child = spawn_raw(&home.0, &[("RUST_LOG", "warn")]);
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    let port = loop {
        if let Ok(chocofactory_core::daemon_lock::LockState::Running(info)) =
            chocofactory_core::daemon_lock::read_lock(&config_root_of(&home))
            && info.port != 0
        {
            break info.port;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // Promises 1000 body bytes, sends 5: the handler blocks on the body.
    conn.write_all(
        b"POST /tasks HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
          Content-Length: 1000\r\n\r\n{\"a\":",
    )
    .await
    .unwrap();
    conn.flush().await.unwrap();
    // Not a fixed sleep: the kernel hands connections to `accept` in order,
    // so once the daemon has answered a second, complete request on a fresh
    // connection it has accepted and registered the stalled one made before
    // it. That narrows the race rather than closing it: "registered" is not
    // "in flight", and the HTTP server can still drop a connection that
    // hasn't yet sent enough bytes to identify its protocol version.
    let client = reqwest::Client::new();
    wait_until(
        "the daemon to answer a request after the stalled one",
        || async {
            match client
                .get(format!("http://127.0.0.1:{port}/projects"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => Ok(()),
                Ok(resp) => Err(format!("status {}", resp.status())),
                Err(err) => Err(err.to_string()),
            }
        },
    )
    .await;

    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");
    let stderr = read_stderr_to_string(&mut child).await;
    assert!(stderr.contains("did not drain"), "{stderr}");
    assert!(lock_is_released(&home));
    drop(conn);
}

#[tokio::test]
async fn piped_stderr_carries_no_ansi_escapes() {
    let home = TempHome::new();
    let mut child = spawn_raw(&home.0, &[("RUST_LOG", "info")]);
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    while !matches!(
        chocofactory_core::daemon_lock::read_lock(&config_root_of(&home)),
        Ok(chocofactory_core::daemon_lock::LockState::Running(ref i)) if i.port != 0
    ) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give startup logging time to land before stopping.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sigterm_and_wait(&mut child).await;
    let stderr = read_stderr_to_string(&mut child).await;
    assert!(
        stderr.contains("listening"),
        "no log output at all: {stderr}"
    );
    assert!(
        !stderr.contains("\x1b["),
        "ANSI escapes in piped stderr: {stderr:?}"
    );
}

// ---- #129: built-in workflows come from the binary ----

/// A fresh `$HOME`: startup writes the built-ins into the private
/// `.builtin-workflows/` copy, never creates `workflows/`, and a `chat` task
/// records a `builtin:` reference.
#[tokio::test]
async fn real_binary_materializes_the_builtins_and_does_not_create_the_old_folder() {
    let daemon = Daemon::spawn().await;
    let root = daemon.home.as_ref().unwrap().0.join(".config/chocofactory");
    let builtins = root.join(".builtin-workflows");

    for (relative, source, _) in chocofactoryd::config_root::builtin_files() {
        assert_eq!(
            std::fs::read_to_string(builtins.join(&relative)).unwrap(),
            source,
            "{relative:?}"
        );
    }
    use std::os::unix::fs::PermissionsExt;
    let script = builtins.join("scripts/open-pr.sh");
    assert!(script.metadata().unwrap().permissions().mode() & 0o111 != 0);
    assert!(builtins.join("README.txt").is_file());
    assert!(
        !root.join("workflows").exists(),
        "the daemon must not create the old workflows folder"
    );

    let (_, project) = daemon.post("/projects", json!({ "name": "demo" })).await;
    let (status, task) = daemon
        .post(
            "/tasks",
            json!({
                "project_id": project["id"].as_str().unwrap(),
                "workflow_def": "chat",
                "title": "t",
                "prompt": "hi",
            }),
        )
        .await;
    assert_eq!(status, 201, "{task}");
    assert_eq!(
        task["workflow_path"],
        format!("builtin:chat@{}", chocofactory_core::version::VERSION)
    );
}

async fn start_daemon_for_report(
    home: &std::path::Path,
    client: &reqwest::Client,
) -> (tokio::process::Child, String) {
    let port = free_port();
    let child = spawn_raw(
        home,
        &[
            ("CHOCOFACTORY_PORT", &port.to_string()),
            ("RUST_LOG", "info"),
        ],
    );
    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        if let Ok(resp) = client.get(format!("{base}/projects")).send().await
            && resp.status().is_success()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon never became ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (child, base)
}

/// A task whose recorded workflow path is inside the old folder is counted by
/// the startup report after a restart.
#[tokio::test]
async fn real_binary_counts_tasks_still_using_the_old_workflows_folder() {
    let home = TempHome::new();
    let old = home.0.join(".config/chocofactory/workflows");
    std::fs::create_dir_all(&old).unwrap();
    let custom = old.join("chat.yaml");
    let chat = chocofactoryd::config_root::builtin_files()
        .into_iter()
        .find(|(relative, _, _)| relative == std::path::Path::new("chat.yaml"))
        .unwrap()
        .1;
    std::fs::write(&custom, chat).unwrap();
    let client = reqwest::Client::new();

    let (mut child, base) = start_daemon_for_report(&home.0, &client).await;
    let project: Value = client
        .post(format!("{base}/projects"))
        .json(&json!({ "name": "demo" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/tasks"))
        .json(&json!({
            "project_id": project["id"].as_str().unwrap(),
            "workflow_file": custom.to_str().unwrap(),
            "title": "t",
            "prompt": "hi",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");

    let (mut child, _base) = start_daemon_for_report(&home.0, &client).await;
    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");
    let stderr = read_stderr_to_string(&mut child).await;
    assert!(
        stderr.contains("1 tasks still use workflows in"),
        "{stderr}"
    );
}

/// Pre-#88 tasks (no recorded path) count as using the old folder only when
/// `<folder>/<workflow_def>.yaml` exists.
#[tokio::test]
async fn real_binary_counts_null_path_tasks_only_when_the_old_file_exists() {
    let home = TempHome::new();
    let cfg = home.0.join(".config/chocofactory");
    let client = reqwest::Client::new();

    let (mut child, base) = start_daemon_for_report(&home.0, &client).await;
    let project: Value = client
        .post(format!("{base}/projects"))
        .json(&json!({ "name": "demo" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/tasks"))
        .json(&json!({
            "project_id": project["id"].as_str().unwrap(),
            "workflow_def": "chat",
            "title": "t",
            "prompt": "hi",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");

    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        cfg.join("chocofactory.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET workflow_path = NULL")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Old folder exists (so the scan runs and reaches the count) but has no
    // chat.yaml: not counted.
    let old = cfg.join("workflows");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("other.yaml"), "name: other\n").unwrap();
    let (mut child, _base) = start_daemon_for_report(&home.0, &client).await;
    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");
    let stderr = read_stderr_to_string(&mut child).await;
    assert!(!stderr.contains("tasks still use workflows in"), "{stderr}");

    // With the old file present: counted.
    std::fs::write(old.join("chat.yaml"), "name: chat\n").unwrap();
    let (mut child, _base) = start_daemon_for_report(&home.0, &client).await;
    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");
    let stderr = read_stderr_to_string(&mut child).await;
    assert!(
        stderr.contains("1 tasks still use workflows in"),
        "{stderr}"
    );
}

/// A `$HOME` that still has the old folder: an edited `chat.yaml` is warned
/// about with both remedies, an identical `coding-task.yaml` is counted as a
/// stale copy, nothing in the folder is touched, and a new `chat` task uses
/// the built-in rather than the edited copy.
#[tokio::test]
async fn real_binary_reports_the_old_workflows_folder_and_leaves_it_alone() {
    let home = TempHome::new();
    let old = home.0.join(".config/chocofactory/workflows");
    std::fs::create_dir_all(&old).unwrap();
    let coding_task = chocofactoryd::config_root::builtin_files()
        .into_iter()
        .find(|(relative, _, _)| relative == std::path::Path::new("coding-task.yaml"))
        .unwrap()
        .1;
    std::fs::write(old.join("coding-task.yaml"), coding_task).unwrap();
    let edited = "name: chat\nstages:\n  other_stage:\n    kind: terminal\n";
    std::fs::write(old.join("chat.yaml"), edited).unwrap();

    let port = free_port();
    let mut child = spawn_raw(
        &home.0,
        &[
            ("CHOCOFACTORY_PORT", &port.to_string()),
            ("RUST_LOG", "info"),
        ],
    );
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        if let Ok(resp) = client.get(format!("{base}/projects")).send().await
            && resp.status().is_success()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon never became ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let project: Value = client
        .post(format!("{base}/projects"))
        .json(&json!({ "name": "demo" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task: Value = client
        .post(format!("{base}/tasks"))
        .json(&json!({
            "project_id": project["id"].as_str().unwrap(),
            "workflow_def": "chat",
            "title": "t",
            "prompt": "hi",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        task["workflow_path"],
        format!("builtin:chat@{}", chocofactory_core::version::VERSION),
        "{task}"
    );

    let status = sigterm_and_wait(&mut child).await;
    assert!(status.success(), "{status:?}");
    let stderr = read_stderr_to_string(&mut child).await;

    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains("WARN")).collect();
    let chat_warnings: Vec<&&str> = warnings
        .iter()
        .filter(|l| l.contains("chat.yaml"))
        .collect();
    assert_eq!(chat_warnings.len(), 1, "{stderr}");
    assert!(
        chat_warnings[0].contains("is no longer read (#129)"),
        "{stderr}"
    );
    assert!(
        chat_warnings[0].contains("choco task create --workflow"),
        "{stderr}"
    );
    assert!(
        chat_warnings[0].contains("move it with its prompts/ and scripts/ into a repo's"),
        "{stderr}"
    );
    assert!(
        !warnings.iter().any(|l| l.contains("coding-task.yaml")),
        "the identical copy is not a warning: {stderr}"
    );
    assert!(
        stderr.contains("ignoring 1 stale copies of built-in workflows"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(old.join("chat.yaml")).unwrap(),
        edited
    );
    assert_eq!(
        std::fs::read_to_string(old.join("coding-task.yaml")).unwrap(),
        coding_task
    );
}
