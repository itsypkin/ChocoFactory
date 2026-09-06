//! Integration tests for the `choco` binary (P1-10, design §6.2). Spawns
//! the *real* `chocofactoryd` binary as a subprocess — the same pattern
//! `chocofactoryd/tests/e2e_smoke.rs` established for issue #42 —
//! with `CHOCOFACTORY_CLAUDE_BINARY` pointed at the real `mock-claude`
//! fixture binary so no real, billable `claude` CLI is ever spawned, and
//! the real `choco` binary against it, asserting on stdout/stderr/exit
//! code. `choco`'s whole job is being an HTTP client for that daemon, so
//! this is the most realistic test available, and needs zero changes to
//! `chocofactoryd` itself.
//!
//! The `workspace_binary`/`free_port`/`TempHome`/`wait_until_ready` helpers
//! below are deliberately duplicated from `e2e_smoke.rs` rather than
//! shared — there's no existing shared test-support crate in this
//! workspace, and introducing one just for ~40 lines would be more
//! machinery than it'd save.

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

struct TempHome(PathBuf);

static UNIQUE: AtomicU64 = AtomicU64::new(0);

impl TempHome {
    fn new() -> Self {
        let suffix = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("choco-cli-test-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempHome(path)
    }

    /// Pre-seeds a workflow definition before the daemon starts — safe
    /// because `seed_builtin_workflows` only ever writes `chat.yaml` if
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
/// (`target/<profile>/deps/<test-exe>` -> `target/<profile>/<name>`).
/// Relies on the workspace's bin targets having already been built (this
/// repo's verification gate always runs `cargo build --workspace
/// --all-targets` before `cargo test --workspace`; plain `cargo test
/// --workspace` also builds every member's own bin target regardless).
fn workspace_binary(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("test binary has no path");
    path.pop();
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

/// Binds an ephemeral port and immediately releases it, so a subprocess
/// started moments later can bind it instead.
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
    _home: TempHome,
}

/// Bounds the `free_port`-race retry (see `Daemon::spawn`) so a run that
/// keeps losing doesn't loop forever.
const MAX_SPAWN_ATTEMPTS: u32 = 5;

impl Daemon {
    async fn spawn(home: TempHome) -> Self {
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
        // because a lost race says nothing about the code under test — a bare
        // `expect` here makes the whole suite flaky under load, which on this
        // repo means a spurious failure on a push that also triggers a paid
        // review run.
        //
        // Only a confirmed bind failure is retried, though: a blanket retry
        // on any startup exit would silently paper over a real crash (e.g. a
        // broken migration), so the child's stderr is piped and checked for
        // the daemon's own bind-failure message before looping.
        let mut last_failure = None;
        for _ in 0..MAX_SPAWN_ATTEMPTS {
            let port = free_port();
            let mut child = Command::new(&daemon_bin)
                .env("HOME", &home.0)
                .env("CHOCOFACTORY_CLAUDE_BINARY", &mock_claude_bin)
                .env("CHOCOFACTORY_PORT", port.to_string())
                .env("RUST_LOG", "error")
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("failed to spawn chocofactoryd");

            let base_url = format!("http://127.0.0.1:{port}");
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
                        _home: home,
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
}

/// Outcome of waiting for the daemon's first successful response.
enum Ready {
    Yes,
    ExitedDuringStartup(std::process::ExitStatus),
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
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[chocofactoryd] {line}");
            }
        });
    }
}

/// Reads a since-exited child's piped stderr to completion. Only called
/// after `try_wait` has already observed the exit, so this can't block on
/// a still-running process.
async fn read_stderr_to_string(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut buf).await;
    }
    buf
}

/// Distinguishes a lost `free_port` race (safe to retry on a fresh port)
/// from any other startup failure (a real bug that must surface, not be
/// retried into silence). Matches the daemon's own `.expect` message at
/// `chocofactoryd/src/main.rs`'s `TcpListener::bind` call.
fn stderr_says_bind_failed(stderr: &str) -> bool {
    stderr.contains("chocofactoryd: failed to bind 127.0.0.1")
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn wait_until_ready(client: &reqwest::Client, base_url: &str, child: &mut Child) -> Ready {
    for _ in 0..100 {
        if let Ok(resp) = client.get(format!("{base_url}/projects")).send().await
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
    panic!("chocofactoryd did not become ready within 5s");
}

struct ChocoOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl ChocoOutput {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|err| panic!("choco stdout wasn't JSON ({err}): {:?}", self.stdout))
    }
}

/// Spawns the real `choco` binary with `--base-url <base_url>` plus
/// `args`, and waits for it to exit. Output is the default human-readable
/// rendering — use [`run_choco_json`] for the machine-facing form.
async fn run_choco(base_url: &str, args: &[&str]) -> ChocoOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_choco"))
        .arg("--base-url")
        .arg(base_url)
        .args(args)
        .output()
        .await
        .expect("failed to spawn choco");
    ChocoOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    }
}

/// Same, with `--json` — the form an agent or script consumes.
async fn run_choco_json(base_url: &str, args: &[&str]) -> ChocoOutput {
    let mut with_json = vec!["--json"];
    with_json.extend_from_slice(args);
    run_choco(base_url, &with_json).await
}

const ECHO_WORKFLOW_YAML: &str = r#"
name: echo-workflow
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

#[tokio::test]
async fn project_create_and_list_round_trip() {
    let daemon = Daemon::spawn(TempHome::new()).await;

    let created = run_choco_json(&daemon.base_url, &["project", "create", "demo"]).await;
    assert_eq!(created.code, Some(0), "stderr: {}", created.stderr);
    let project = created.json();
    assert_eq!(project["name"], "demo");
    let project_id = project["id"].as_str().unwrap().to_string();

    let listed = run_choco_json(&daemon.base_url, &["project", "list"]).await;
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    let projects = listed.json();
    assert!(
        projects
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == project_id),
        "created project not present in `project list`: {projects:?}"
    );
}

/// Also proves the `--workflow` correction (design §6.2 vs. §2.2): a
/// custom, non-builtin workflow name is accepted and started, not
/// rejected by a `chat|coding_task` enum.
#[tokio::test]
async fn task_create_with_a_custom_workflow_and_parent_task_tags_delegation() {
    let home = TempHome::new();
    home.write_workflow("echo-workflow", ECHO_WORKFLOW_YAML);
    let daemon = Daemon::spawn(home).await;

    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let parent = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "echo-workflow",
            "--title",
            "parent",
            "--prompt",
            "hello",
        ],
    )
    .await;
    assert_eq!(parent.code, Some(0), "stderr: {}", parent.stderr);
    let parent_id = parent.json()["id"].as_str().unwrap().to_string();

    let child = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "echo-workflow",
            "--title",
            "child",
            "--prompt",
            "hi",
            "--parent-task",
            &parent_id,
        ],
    )
    .await;
    assert_eq!(child.code, Some(0), "stderr: {}", child.stderr);
    let child_task = child.json();
    assert_eq!(child_task["parent_task_id"], parent_id);
    let child_id = child_task["id"].as_str().unwrap().to_string();

    let status = run_choco_json(&daemon.base_url, &["task", "status", &child_id]).await;
    assert_eq!(status.code, Some(0), "stderr: {}", status.stderr);
    let detail = status.json();
    assert_eq!(detail["parent_task_id"], parent_id);
    assert_eq!(detail["workflow_state"]["current_stage"], "chatting");
}

#[tokio::test]
async fn task_send_confirms_in_human_mode_and_stays_silent_under_json() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();
    let task = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hello",
        ],
    )
    .await
    .json();
    let task_id = task["id"].as_str().unwrap().to_string();

    let send = run_choco(
        &daemon.base_url,
        &["task", "send", &task_id, "--text", "again"],
    )
    .await;
    assert_eq!(send.code, Some(0), "stderr: {}", send.stderr);
    assert!(
        send.stdout.contains("accepted"),
        "human mode should confirm the send landed: {:?}",
        send.stdout
    );

    // The endpoint answers 202 with no body, so there is nothing for a
    // script to parse — `--json` emits nothing rather than a courtesy
    // message that would break `| jq`.
    let send = run_choco_json(
        &daemon.base_url,
        &["task", "send", &task_id, "--text", "once more"],
    )
    .await;
    assert_eq!(send.code, Some(0), "stderr: {}", send.stderr);
    assert_eq!(send.stdout, "");
}

#[tokio::test]
async fn task_list_filters_by_project_id() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project_a = run_choco_json(&daemon.base_url, &["project", "create", "a"])
        .await
        .json();
    let project_a_id = project_a["id"].as_str().unwrap().to_string();
    let project_b = run_choco_json(&daemon.base_url, &["project", "create", "b"])
        .await
        .json();
    let project_b_id = project_b["id"].as_str().unwrap().to_string();

    for (project_id, title) in [(&project_a_id, "ta"), (&project_b_id, "tb")] {
        let out = run_choco(
            &daemon.base_url,
            &[
                "task",
                "create",
                "--project",
                project_id,
                "--workflow",
                "chat",
                "--title",
                title,
                "--prompt",
                "hi",
            ],
        )
        .await;
        assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    }

    let listed = run_choco_json(
        &daemon.base_url,
        &["task", "list", "--project", &project_a_id],
    )
    .await;
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    let tasks = listed.json();
    let tasks = tasks.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["project_id"], project_a_id);
}

/// Proves the `--status` correction: it's a plain string, not an
/// `open|closed` enum. An enum would reject this value at parse time
/// (clap usage error, exit 2) before a request is ever sent; instead it
/// reaches the daemon and simply matches nothing.
#[tokio::test]
async fn task_list_accepts_an_arbitrary_status_value() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let out = run_choco_json(
        &daemon.base_url,
        &["task", "list", "--status", "definitely-not-a-real-status"],
    )
    .await;
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.json().as_array().unwrap().len(), 0);
}

/// Proves the `--workflow` correction from the other direction: an
/// unrecognized workflow name is accepted by the CLI (not validated
/// client-side against a fixed set) and surfaces as the daemon's own 404.
#[tokio::test]
async fn task_create_with_an_unknown_workflow_surfaces_the_daemons_404() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let out = run_choco(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "ghost",
            "--title",
            "t",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(out.stderr.starts_with("error: "), "stderr: {}", out.stderr);
}

#[tokio::test]
async fn task_status_of_an_unknown_task_id_is_reported_as_an_error() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let out = run_choco(&daemon.base_url, &["task", "status", "does-not-exist"]).await;
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(out.stderr.starts_with("error: "), "stderr: {}", out.stderr);
}

#[tokio::test]
async fn reports_a_clear_error_when_the_daemon_is_unreachable() {
    // Bound then released — nothing listens here, and no daemon is spawned.
    let port = free_port();
    let base_url = format!("http://127.0.0.1:{port}");

    let out = run_choco(&base_url, &["project", "list"]).await;
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(&format!(
            "error: failed to connect to chocofactoryd at {base_url} (is it running?): "
        )),
        "stderr: {}",
        out.stderr
    );
}

/// Default output is a human-readable summary, not raw JSON — and the
/// same call under `--json` is machine-parseable. Both halves matter:
/// `choco` is human-scriptable *and* agent-callable (design Q12).
#[tokio::test]
async fn default_output_is_human_readable_and_json_is_opt_in() {
    let daemon = Daemon::spawn(TempHome::new()).await;

    let human = run_choco(&daemon.base_url, &["project", "create", "demo"]).await;
    assert_eq!(human.code, Some(0), "stderr: {}", human.stderr);
    assert!(
        human.stdout.contains("Name") && human.stdout.contains("demo"),
        "expected a labelled summary: {:?}",
        human.stdout
    );
    assert!(
        serde_json::from_str::<Value>(human.stdout.trim()).is_err(),
        "default output should not be raw JSON: {:?}",
        human.stdout
    );

    let json = run_choco_json(&daemon.base_url, &["project", "list"]).await;
    assert_eq!(json.code, Some(0), "stderr: {}", json.stderr);
    assert_eq!(json.json().as_array().unwrap().len(), 1);
}

/// `--project` takes a name, not just an id — the id path still works, so
/// existing scripted callers are unaffected.
#[tokio::test]
async fn task_create_accepts_a_project_name_as_well_as_an_id() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "my-project"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let by_name = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            "my-project",
            "--workflow",
            "chat",
            "--title",
            "named",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(by_name.code, Some(0), "stderr: {}", by_name.stderr);
    assert_eq!(by_name.json()["project_id"], project_id);

    let by_id = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "chat",
            "--title",
            "by-id",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(by_id.code, Some(0), "stderr: {}", by_id.stderr);
    assert_eq!(by_id.json()["project_id"], project_id);

    // Filtering by name works too, not just at task-creation time.
    let listed = run_choco_json(
        &daemon.base_url,
        &["task", "list", "--project", "my-project"],
    )
    .await;
    assert_eq!(listed.json().as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn an_unknown_project_name_is_a_clear_error() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let out = run_choco(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            "nope",
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("no project named 'nope'"),
        "stderr: {}",
        out.stderr
    );
}

/// `projects.name` has no unique constraint, so a duplicated name must
/// report the candidates rather than silently picking one.
#[tokio::test]
async fn an_ambiguous_project_name_is_rejected_with_the_candidate_ids() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let first = run_choco_json(&daemon.base_url, &["project", "create", "dupe"])
        .await
        .json();
    let second = run_choco_json(&daemon.base_url, &["project", "create", "dupe"])
        .await
        .json();
    let (first_id, second_id) = (
        first["id"].as_str().unwrap().to_string(),
        second["id"].as_str().unwrap().to_string(),
    );

    let out = run_choco(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            "dupe",
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("matches 2 projects"), "{}", out.stderr);
    assert!(out.stderr.contains(&first_id), "{}", out.stderr);
    assert!(out.stderr.contains(&second_id), "{}", out.stderr);

    // ...and passing the id resolves the ambiguity.
    let ok = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &first_id,
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hi",
        ],
    )
    .await;
    assert_eq!(ok.code, Some(0), "stderr: {}", ok.stderr);
}

#[tokio::test]
async fn task_events_shows_the_conversation() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let task = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            project["id"].as_str().unwrap(),
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hello there",
        ],
    )
    .await
    .json();
    let task_id = task["id"].as_str().unwrap().to_string();

    // mock-claude echoes `echo:{text}`; wait for that reply to land so the
    // assertion covers a real round trip, not just the human_message.
    let mut human = ChocoOutput {
        stdout: String::new(),
        stderr: String::new(),
        code: None,
    };
    for _ in 0..100 {
        human = run_choco(&daemon.base_url, &["task", "events", &task_id]).await;
        if human.stdout.contains("echo:hello there") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(human.code, Some(0), "stderr: {}", human.stderr);
    assert!(
        human.stdout.contains("human_message") && human.stdout.contains("hello there"),
        "expected the prompt in the transcript: {:?}",
        human.stdout
    );
    assert!(
        human.stdout.contains("echo:hello there"),
        "expected the agent's reply in the transcript: {:?}",
        human.stdout
    );

    let json = run_choco_json(&daemon.base_url, &["task", "events", &task_id]).await;
    let page = json.json();
    assert!(!page["events"].as_array().unwrap().is_empty());

    // `--limit` is passed through to the daemon's own pagination, which
    // hands back a token to continue from.
    let limited = run_choco_json(
        &daemon.base_url,
        &["task", "events", &task_id, "--limit", "1"],
    )
    .await
    .json();
    assert_eq!(limited["events"].as_array().unwrap().len(), 1);
    let token = limited["next_token"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a continuation token: {limited:?}"));

    // Feeding the token back is the exact move the human view tells users
    // to make ("continue with `--after <token>`"), so it gets exercised
    // end to end rather than only being printed.
    let next = run_choco_json(
        &daemon.base_url,
        &["task", "events", &task_id, "--limit", "1", "--after", token],
    )
    .await
    .json();
    let first_id = limited["events"][0]["id"].as_str().unwrap();
    let next_id = next["events"][0]["id"].as_str().unwrap();
    assert_ne!(
        first_id, next_id,
        "--after should advance past the first page, got the same event twice"
    );
}

#[tokio::test]
async fn task_events_for_an_unknown_task_is_an_error() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let out = run_choco(&daemon.base_url, &["task", "events", "does-not-exist"]).await;
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.starts_with("error: "), "stderr: {}", out.stderr);
}

/// The headline fix for `task status`: the stage trail carries the outcome
/// that caused each hop and when it happened, and the human view renders
/// it as a timeline rather than a bare list of names.
#[tokio::test]
async fn task_status_shows_stage_progress_with_outcomes_and_timestamps() {
    let home = TempHome::new();
    home.write_workflow(
        "gated",
        r#"
name: gated
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
    let daemon = Daemon::spawn(home).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let task = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            project["id"].as_str().unwrap(),
            "--workflow",
            "gated",
            "--title",
            "gated task",
            "--prompt",
            "start",
        ],
    )
    .await
    .json();
    let task_id = task["id"].as_str().unwrap().to_string();

    // Before any transition the trail already has the stage the task
    // started in — `stage_history` never recorded that, because it only
    // appended on the way *out* of a stage (X-3).
    let mut fresh = run_choco(&daemon.base_url, &["task", "status", &task_id]).await;
    for _ in 0..100 {
        if fresh.stdout.contains("gate (start)") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        fresh = run_choco(&daemon.base_url, &["task", "status", &task_id]).await;
    }
    assert!(
        fresh.stdout.contains("1. gate (start)"),
        "expected the entry stage in the trail: {:?}",
        fresh.stdout
    );

    // Sending into a human_gate resumes it, moving gate -> review.
    let sent = run_choco(
        &daemon.base_url,
        &["task", "send", &task_id, "--text", "go"],
    )
    .await;
    assert_eq!(sent.code, Some(0), "stderr: {}", sent.stderr);

    // `task send` is accepted with a 202 and the transition runs behind it,
    // so poll for the hop rather than assuming it has already landed.
    //
    // The break condition is deliberately *identical* to the assertion
    // below, not a weaker proxy for it: `GET /tasks/:id` reads
    // `workflow_state` before `stage_trail`, so a response can pair
    // `current_stage == "gate"` with a trail already ending at `review`
    // (the skew documented at `api/tasks.rs`). That renders the hop line
    // without `(current)` — enough to satisfy a break keyed on the hop
    // alone, and then fail the assertion. Polling for exactly what's
    // asserted costs one extra poll instead.
    let settled = |out: &str| {
        out.lines()
            .any(|l| l.contains("gate --[resumed]--> review") && l.contains("(current)"))
    };
    let mut status = run_choco(&daemon.base_url, &["task", "status", &task_id]).await;
    for _ in 0..100 {
        if settled(&status.stdout) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        status = run_choco(&daemon.base_url, &["task", "status", &task_id]).await;
    }
    assert_eq!(status.code, Some(0), "stderr: {}", status.stderr);
    // The stage the task entered is the last trail entry, so it's marked in
    // place instead of repeated on a trailing `→` line. Asserted against
    // the whole line, not a bare "(current)": the duplicate-line regression
    // this guards against would still contain that substring.
    assert!(
        settled(&status.stdout),
        "expected the hop with its outcome, marked current on the same line: {:?}",
        status.stdout
    );
    assert!(
        !status.stdout.contains("→ review"),
        "the current stage should not also get a trailing arrow line: {:?}",
        status.stdout
    );

    // The same data is present structurally under --json, with a real
    // timestamp — the CLI renders it, the daemon records it. It arrives as
    // `stage_trail` alongside `workflow_state`, one `stage_entered` event
    // per stage entered, rather than as the departed-stage edges the old
    // `workflow_state.stage_history` held.
    let detail = run_choco_json(&daemon.base_url, &["task", "status", &task_id])
        .await
        .json();
    assert!(
        detail["workflow_state"]["stage_history"].is_null(),
        "stage_history should be gone: {detail}"
    );
    let trail = detail["stage_trail"].as_array().unwrap();
    assert_eq!(trail.len(), 2, "{trail:?}");
    assert_eq!(trail[0]["payload"]["stage"], "gate");
    assert_eq!(trail[0]["payload"]["outcome"], serde_json::Value::Null);
    assert_eq!(trail[1]["payload"]["stage"], "review");
    assert_eq!(trail[1]["payload"]["outcome"], "resumed");
    // Task-scoped, not session-scoped: `gate` is a human_gate and opens no
    // agent session at all, which the pre-X-3 schema could not represent.
    for entry in trail {
        assert_eq!(entry["event_type"], "stage_entered");
        assert_eq!(entry["task_id"], task_id.as_str());
        assert!(entry["created_at"].is_string(), "{entry}");
    }
    assert_eq!(trail[0]["task_run_id"], serde_json::Value::Null);
}

// ---- P2-6 per-role config overrides (#17) ----

/// A two-role workflow. `coding` is a `human_gate` so the task parks
/// immediately: this test is about the config flags, and parking keeps it
/// from depending on an agent subprocess completing.
const TWO_ROLE_WORKFLOW_YAML: &str = r#"
name: two-role
roles:
  coder:
    cli: claude
    model: sonnet
  reviewer:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: human_gate
    on: { resumed: internal_review }
  internal_review:
    kind: agent_turn
    role: reviewer
    on: {}
"#;

/// The end-to-end shape of #17's CLI half: several roles configured
/// independently in a single `task create`, then changed after the fact by
/// `task reconfigure` without disturbing the rest of the config.
#[tokio::test]
async fn task_create_and_reconfigure_set_overrides_for_more_than_one_role() {
    let home = TempHome::new();
    home.write_workflow("two-role", TWO_ROLE_WORKFLOW_YAML);
    let daemon = Daemon::spawn(home).await;

    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let created = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "two-role",
            "--title",
            "t",
            "--prompt",
            "hello",
            "--repo",
            ".",
            "--role-model",
            "coder=opus",
            "--role-model",
            "reviewer=haiku",
            "--role-cli",
            "reviewer=claude",
        ],
    )
    .await;
    assert_eq!(created.code, Some(0), "stderr: {}", created.stderr);
    let task = created.json();
    let task_id = task["id"].as_str().unwrap().to_string();

    // Both roles landed, each under its own key, alongside the task-wide cwd.
    assert_eq!(task["config"]["roles"]["coder"]["model"], "opus");
    assert_eq!(task["config"]["roles"]["reviewer"]["model"], "haiku");
    assert_eq!(task["config"]["roles"]["reviewer"]["cli"], "claude");
    assert_eq!(task["config"]["cwd"], ".");

    // Reconfigure one field of one role; everything else must survive.
    let patched = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "reconfigure",
            &task_id,
            "--role-model",
            "coder=sonnet",
        ],
    )
    .await;
    assert_eq!(patched.code, Some(0), "stderr: {}", patched.stderr);
    let patched = patched.json();
    assert_eq!(patched["config"]["roles"]["coder"]["model"], "sonnet");
    assert_eq!(patched["config"]["roles"]["reviewer"]["model"], "haiku");
    assert_eq!(patched["config"]["roles"]["reviewer"]["cli"], "claude");
    assert_eq!(patched["config"]["cwd"], ".");
}

/// Human (non-`--json`) output has to name every configured role, or an
/// override is invisible unless the caller re-reads the raw JSON.
#[tokio::test]
async fn task_create_human_output_names_each_configured_role() {
    let home = TempHome::new();
    home.write_workflow("two-role", TWO_ROLE_WORKFLOW_YAML);
    let daemon = Daemon::spawn(home).await;

    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let created = run_choco(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "two-role",
            "--title",
            "t",
            "--prompt",
            "hello",
            "--role-model",
            "coder=opus",
            "--role-model",
            "reviewer=haiku",
        ],
    )
    .await;
    assert_eq!(created.code, Some(0), "stderr: {}", created.stderr);
    assert!(
        created.stdout.contains("coder: model=opus"),
        "stdout: {}",
        created.stdout
    );
    assert!(
        created.stdout.contains("reviewer: model=haiku"),
        "stdout: {}",
        created.stdout
    );
}

/// A malformed flag fails before anything is created, with a message naming
/// the offending flag — not a silently-ignored override.
#[tokio::test]
async fn task_create_rejects_a_malformed_role_flag_without_creating_a_task() {
    let home = TempHome::new();
    home.write_workflow("two-role", TWO_ROLE_WORKFLOW_YAML);
    let daemon = Daemon::spawn(home).await;

    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let failed = run_choco(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "two-role",
            "--title",
            "t",
            "--prompt",
            "hello",
            // No `=`, so there's no way to tell which role was meant.
            "--role-model",
            "opus",
        ],
    )
    .await;
    assert_eq!(failed.code, Some(1), "stdout: {}", failed.stdout);
    assert!(
        failed.stderr.contains("--role-model"),
        "stderr: {}",
        failed.stderr
    );

    // Nothing was created — the flags are validated before the request.
    let listed = run_choco_json(&daemon.base_url, &["task", "list"])
        .await
        .json();
    assert_eq!(listed.as_array().unwrap().len(), 0, "{listed}");
}

/// `reconfigure` with no overrides is a mistake worth reporting, not a
/// silent no-op request.
#[tokio::test]
async fn task_reconfigure_without_any_overrides_is_an_error() {
    let daemon = Daemon::spawn(TempHome::new()).await;

    let failed = run_choco(&daemon.base_url, &["task", "reconfigure", "some-task"]).await;
    assert_eq!(failed.code, Some(1), "stdout: {}", failed.stdout);
    assert!(
        failed.stderr.contains("nothing to change"),
        "stderr: {}",
        failed.stderr
    );
}

/// `task cancel` end to end through the real binaries (#69).
#[tokio::test]
async fn task_cancel_confirms_in_human_mode_and_stays_silent_under_json() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let make_task = || async {
        run_choco_json(
            &daemon.base_url,
            &[
                "task",
                "create",
                "--project",
                &project_id,
                "--workflow",
                "chat",
                "--title",
                "t",
                "--prompt",
                "hello",
            ],
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let task_id = make_task().await;
    let cancelled = run_choco(&daemon.base_url, &["task", "cancel", &task_id]).await;
    assert_eq!(cancelled.code, Some(0), "stderr: {}", cancelled.stderr);
    assert!(
        cancelled.stdout.contains("cancelled"),
        "human mode should confirm the cancel landed: {:?}",
        cancelled.stdout
    );

    let status = run_choco_json(&daemon.base_url, &["task", "status", &task_id])
        .await
        .json();
    assert_eq!(status["status"], "cancelled");

    // 202 with no body, so `--json` emits nothing rather than a courtesy
    // message that would break `| jq` — same contract as `task send`.
    let other = make_task().await;
    let cancelled = run_choco_json(&daemon.base_url, &["task", "cancel", &other]).await;
    assert_eq!(cancelled.code, Some(0), "stderr: {}", cancelled.stderr);
    assert_eq!(cancelled.stdout, "");
}

/// A cancelled task is a dead end, and the CLI should say so rather than
/// appearing to accept a message the daemon dropped.
#[tokio::test]
async fn sending_to_a_cancelled_task_fails_with_a_clear_message() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco_json(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();
    let task_id = run_choco_json(
        &daemon.base_url,
        &[
            "task",
            "create",
            "--project",
            &project_id,
            "--workflow",
            "chat",
            "--title",
            "t",
            "--prompt",
            "hello",
        ],
    )
    .await
    .json()["id"]
        .as_str()
        .unwrap()
        .to_string();

    run_choco(&daemon.base_url, &["task", "cancel", &task_id]).await;

    let failed = run_choco(
        &daemon.base_url,
        &["task", "send", &task_id, "--text", "hello?"],
    )
    .await;
    assert_eq!(failed.code, Some(1), "stdout: {}", failed.stdout);
    assert!(
        failed.stderr.contains("cancelled"),
        "stderr should explain the task was cancelled: {}",
        failed.stderr
    );
}
