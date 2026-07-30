//! Integration tests for the `choco` binary (P1-10, design §6.2). Spawns
//! the *real* `chokofactoryd` binary as a subprocess — the same pattern
//! `chokofactoryd/tests/e2e_smoke.rs` established for issue #42 —
//! with `CHOKOFACTORY_CLAUDE_BINARY` pointed at the real `mock-claude`
//! fixture binary so no real, billable `claude` CLI is ever spawned, and
//! the real `choco` binary against it, asserting on stdout/stderr/exit
//! code. `choco`'s whole job is being an HTTP client for that daemon, so
//! this is the most realistic test available, and needs zero changes to
//! `chokofactoryd` itself.
//!
//! The `workspace_binary`/`free_port`/`TempHome`/`wait_until_ready` helpers
//! below are deliberately duplicated from `e2e_smoke.rs` rather than
//! shared — there's no existing shared test-support crate in this
//! workspace, and introducing one just for ~40 lines would be more
//! machinery than it'd save.

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
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

/// Drives the real `chokofactoryd` binary as a subprocess.
struct Daemon {
    child: Child,
    base_url: String,
    _home: TempHome,
}

impl Daemon {
    async fn spawn(home: TempHome) -> Self {
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
        let client = reqwest::Client::new();
        wait_until_ready(&client, &base_url, &mut child).await;

        Daemon {
            child,
            base_url,
            _home: home,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
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
/// `args`, and waits for it to exit.
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

    let created = run_choco(&daemon.base_url, &["project", "create", "demo"]).await;
    assert_eq!(created.code, Some(0), "stderr: {}", created.stderr);
    let project = created.json();
    assert_eq!(project["name"], "demo");
    let project_id = project["id"].as_str().unwrap().to_string();

    let listed = run_choco(&daemon.base_url, &["project", "list"]).await;
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

    let project = run_choco(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();

    let parent = run_choco(
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

    let child = run_choco(
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

    let status = run_choco(&daemon.base_url, &["task", "status", &child_id]).await;
    assert_eq!(status.code, Some(0), "stderr: {}", status.stderr);
    let detail = status.json();
    assert_eq!(detail["parent_task_id"], parent_id);
    assert_eq!(detail["workflow_state"]["current_stage"], "chatting");
}

#[tokio::test]
async fn task_send_succeeds_with_empty_stdout() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project = run_choco(&daemon.base_url, &["project", "create", "demo"])
        .await
        .json();
    let project_id = project["id"].as_str().unwrap().to_string();
    let task = run_choco(
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
    assert_eq!(send.stdout, "");
}

#[tokio::test]
async fn task_list_filters_by_project_id() {
    let daemon = Daemon::spawn(TempHome::new()).await;
    let project_a = run_choco(&daemon.base_url, &["project", "create", "a"])
        .await
        .json();
    let project_a_id = project_a["id"].as_str().unwrap().to_string();
    let project_b = run_choco(&daemon.base_url, &["project", "create", "b"])
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

    let listed = run_choco(
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
    let out = run_choco(
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
    let project = run_choco(&daemon.base_url, &["project", "create", "demo"])
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
            "error: failed to connect to chokofactoryd at {base_url} (is it running?): "
        )),
        "stderr: {}",
        out.stderr
    );
}
