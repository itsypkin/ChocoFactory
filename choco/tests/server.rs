//! `choco server start|stop|restart|status` against the real `choco` and the
//! real sibling `chocofactoryd` in `target/debug/` (#84 part 2).
//!
//! Safety rules, per test: its own temporary `HOME`, `--port 0`,
//! `CHOCOFACTORY_CLAUDE_BINARY` set to a fixture, `CHOCO_BASE_URL` removed.
//! Non-`server` commands only run after the temp HOME's lock shows a daemon
//! the test started. Daemons are signalled only by the pid in that lock.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chocofactory_core::daemon_lock::{LockState, read_lock};
use serde_json::Value;

static UNIQUE: AtomicU64 = AtomicU64::new(0);
const DEADLINE: Duration = Duration::from_secs(60);

fn target_dir() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path
}

struct Env {
    home: PathBuf,
    claude: PathBuf,
}

impl Env {
    fn new() -> Self {
        Self::with_claude(target_dir().join("mock-claude"))
    }

    fn with_claude(claude: PathBuf) -> Self {
        let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("choco-server-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        Env { home, claude }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".config").join("chocofactory")
    }

    fn choco_at(&self, bin: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("HOME", &self.home)
            .env("CHOCOFACTORY_CLAUDE_BINARY", &self.claude)
            .env("RUST_LOG", "error")
            .env_remove("CHOCO_BASE_URL");
        cmd.output().expect("failed to run choco")
    }

    fn choco(&self, args: &[&str]) -> Output {
        self.choco_at(&target_dir().join("choco"), args)
    }

    fn lock(&self) -> LockState {
        read_lock(&self.root()).unwrap()
    }

    fn running(&self) -> chocofactory_core::daemon_lock::LockInfo {
        match self.lock() {
            LockState::Running(info) => info,
            other => panic!("expected a running daemon, got {other:?}"),
        }
    }

    /// Starts a daemon and checks it is not on the real daemon's port.
    fn start(&self) -> chocofactory_core::daemon_lock::LockInfo {
        let out = self.choco(&["server", "start", "--port", "0"]);
        assert!(out.status.success(), "start failed: {}", text(&out));
        let info = self.running();
        assert_ne!(info.port, 4141, "test daemon must not use the real port");
        info
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.choco(&a);
        assert!(out.status.success(), "{args:?} failed: {}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Ok(LockState::Running(info)) = read_lock(&self.root()) {
            unsafe { libc::kill(info.pid as i32, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + DEADLINE;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// A fixture "claude" that never finishes a turn.
fn blocking_claude(env_home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = env_home.join("blocking-claude.sh");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const AGENT_WF: &str = r#"
name: blocking
roles:
  worker:
    cli: claude
    model: claude-sonnet-5-5
stages:
  work:
    kind: agent_turn
    role: worker
    on: { ok: done }
  done:
    kind: terminal
"#;

const POLL_WF: &str = r#"
name: waiting
stages:
  polling:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: 3600s
    outcomes:
      - match: "NEVER"
        then: green
    on: { green: done, timeout: done }
  done:
    kind: terminal
"#;

/// Creates a project and a task on the already-running temp daemon.
fn create_task(env: &Env, workflow: &str) -> String {
    let wf = env.home.join("wf.yaml");
    std::fs::write(&wf, workflow).unwrap();
    let repo = env.home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    env.json(&[
        "project",
        "create",
        "demo",
        "--repo",
        repo.to_str().unwrap(),
    ]);
    let task = env.json(&[
        "task",
        "create",
        "--project",
        "demo",
        "--workflow",
        wf.to_str().unwrap(),
        "--title",
        "t",
        "--prompt",
        "go",
    ]);
    task["id"].as_str().unwrap().to_string()
}

fn wait_in_flight(env: &Env) {
    wait_for("an in-flight turn", || {
        let out = env.choco(&["--json", "server", "status"]);
        serde_json::from_slice::<Value>(&out.stdout)
            .map(|v| {
                !v["daemon"]["in_flight"]
                    .as_array()
                    .is_none_or(|a| a.is_empty())
            })
            .unwrap_or(false)
    });
}

#[test]
fn start_status_log_and_session() {
    let env = Env::new();
    let info = env.start();
    let status = env.json(&["server", "status"]);
    assert_eq!(status["running"], true);
    assert_eq!(status["daemon"]["pid"], info.pid);
    assert_eq!(status["daemon"]["version"], status["choco"]["version"]);
    let log = std::fs::read_to_string(env.root().join("logs/chocofactoryd.log")).unwrap();
    assert!(log.contains("=== choco server start"), "{log}");
    assert!(log.len() > 60, "daemon startup lines missing: {log}");
    assert_eq!(unsafe { libc::getsid(info.pid as i32) }, info.pid as i32);
    // Human status renders too.
    let human = env.choco(&["server", "status"]);
    assert_eq!(human.status.code(), Some(0));
    assert!(stdout(&human).contains("running"), "{}", text(&human));
}

#[test]
fn start_when_running_is_a_noop() {
    let env = Env::new();
    let info = env.start();
    let out = env.choco(&["server", "start"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("already running"), "{}", text(&out));
    assert_eq!(env.running().pid, info.pid);
}

#[test]
fn start_fails_when_port_is_taken() {
    let env = Env::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let began = Instant::now();
    let out = env.choco(&["server", "start", "--port", &port]);
    assert!(!out.status.success());
    assert!(began.elapsed() < Duration::from_secs(30));
    assert!(stderr(&out).contains("failed to bind"), "{}", text(&out));
}

#[test]
fn default_base_url_follows_the_lock_file() {
    let env = Env::new();
    env.start();
    let out = env.choco(&["project", "list"]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn stop_when_not_running() {
    let env = Env::new();
    let out = env.choco(&["server", "stop"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("not running"));
}

#[test]
fn stop_refuses_while_an_agent_turn_runs_and_force_parks_it() {
    let probe = Env::new();
    let claude = blocking_claude(&probe.home);
    let env = Env {
        home: probe.home.clone(),
        claude,
    };
    std::mem::forget(probe); // `env` owns the same directory now
    env.start();
    let id = create_task(&env, AGENT_WF);
    wait_in_flight(&env);

    let out = env.choco(&["server", "stop"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("(agent_turn)") && all.contains(&id), "{all}");
    assert_eq!(env.json(&["server", "status"])["running"], true);

    let out = env.choco(&["server", "stop", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(matches!(env.lock(), LockState::NotRunning { .. }));

    env.start();
    wait_for("the task to be stuck", || {
        let detail = env.json(&["task", "status", &id]);
        detail["status"] == "stuck" || detail["task"]["status"] == "stuck"
    });
}

#[test]
fn stop_allows_a_waiting_poll_without_force() {
    let env = Env::new();
    env.start();
    create_task(&env, POLL_WF);
    let out = env.choco(&["server", "stop"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(matches!(env.lock(), LockState::NotRunning { .. }));
}

#[test]
fn stale_lock_reads_as_not_running_and_start_recovers() {
    let env = Env::new();
    let info = env.start();
    unsafe { libc::kill(info.pid as i32, libc::SIGKILL) };
    wait_for("the lock to be released", || {
        matches!(env.lock(), LockState::NotRunning { .. })
    });
    let out = env.choco(&["server", "status"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let so = stdout(&out);
    assert!(so.contains("not running"), "{so}");
    assert!(
        so.contains(&format!("last ran as pid {}", info.pid)),
        "{so}"
    );
    env.start();
}

#[test]
fn restart_keeps_the_port_and_changes_the_pid() {
    let env = Env::new();
    let first = env.start();
    let out = env.choco(&["server", "restart"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let second = env.running();
    assert_eq!(second.port, first.port);
    assert_ne!(second.pid, first.pid);
}

#[test]
fn restart_when_not_running_starts_it() {
    let env = Env::new();
    let out = env.choco(&["server", "restart", "--port", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(stdout(&out).contains("was not running"));
    assert_ne!(env.running().port, 4141);
}

#[test]
fn status_json_when_not_running() {
    let env = Env::new();
    let out = env.choco(&["--json", "server", "status"]);
    assert_eq!(out.status.code(), Some(3));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["running"], false);
    assert!(v["daemon"].is_null());
    assert!(v["choco"]["version"].is_string());
}

async fn skew_server(version: Option<&'static str>) -> String {
    use axum::{Router, routing::get};
    let app = Router::new().route(
        "/projects",
        get(move || async move {
            let mut headers = axum::http::HeaderMap::new();
            if let Some(v) = version {
                headers.insert("x-chocofactory-version", v.parse().unwrap());
            }
            (headers, "[]")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[tokio::test]
async fn version_skew_warning_prints_once_on_stderr() {
    let env = Env::new();
    let url = skew_server(Some("0.0.0-test")).await;
    let out = tokio::task::spawn_blocking(move || {
        let o = env.choco(&["--base-url", &url, "--json", "project", "list"]);
        (env, o)
    })
    .await
    .unwrap()
    .1;
    assert!(out.status.success(), "{}", text(&out));
    let err = stderr(&out);
    assert_eq!(err.matches("warning: choco").count(), 1, "{err}");
    assert!(err.contains("chocofactoryd 0.0.0-test"), "{err}");
    serde_json::from_slice::<Value>(&out.stdout).expect("stdout stays JSON");
}

#[tokio::test]
async fn missing_version_header_warns() {
    let env = Env::new();
    let url = skew_server(None).await;
    let out = tokio::task::spawn_blocking(move || {
        let o = env.choco(&["--base-url", &url, "--json", "project", "list"]);
        (env, o)
    })
    .await
    .unwrap()
    .1;
    assert!(
        stderr(&out).contains("doesn't report a version"),
        "{}",
        text(&out)
    );
    serde_json::from_slice::<Value>(&out.stdout).expect("stdout stays JSON");
}

#[test]
fn oversized_log_is_rotated() {
    let env = Env::new();
    let logs = env.root().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let big = vec![b'x'; 11 * 1024 * 1024];
    std::fs::write(logs.join("chocofactoryd.log"), &big).unwrap();
    env.start();
    assert_eq!(
        std::fs::read(logs.join("chocofactoryd.log.1")).unwrap(),
        big
    );
    assert!(
        std::fs::metadata(logs.join("chocofactoryd.log"))
            .unwrap()
            .len()
            < 100_000
    );
}

#[test]
fn start_without_a_sibling_daemon_fails() {
    let env = Env::new();
    let lone = env.home.join("lone");
    std::fs::create_dir_all(&lone).unwrap();
    std::fs::copy(target_dir().join("choco"), lone.join("choco")).unwrap();
    let out = env.choco_at(&lone.join("choco"), &["server", "start", "--port", "0"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("chocofactoryd not found next to choco"),
        "{}",
        text(&out)
    );
}
