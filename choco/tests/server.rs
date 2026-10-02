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
        // Non-`server` commands always get an explicit URL read from this
        // HOME's lock, so a dead test daemon can never send them to the
        // operator's real daemon on :4141 (`running()` panics instead).
        let base = format!("http://127.0.0.1:{}", self.running().port);
        let mut a = vec!["--json"];
        if args.first() != Some(&"server") {
            a.extend_from_slice(&["--base-url", base.as_str()]);
        }
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
    // Start by hand with info-level logging so the daemon's startup lines
    // land in the log (the shared helper pins RUST_LOG=error).
    let out = Command::new(target_dir().join("choco"))
        .args(["server", "start", "--port", "0"])
        .env("HOME", &env.home)
        .env("CHOCOFACTORY_CLAUDE_BINARY", &env.claude)
        .env("RUST_LOG", "info")
        .env_remove("CHOCO_BASE_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let info = env.running();
    assert_ne!(info.port, 4141);
    let status = env.json(&["server", "status"]);
    assert_eq!(status["running"], true);
    assert_eq!(status["daemon"]["pid"], info.pid);
    assert_eq!(status["daemon"]["version"], status["choco"]["version"]);
    let log = std::fs::read_to_string(env.root().join("logs/chocofactoryd.log")).unwrap();
    assert!(log.contains("=== choco server start"), "{log}");
    assert!(
        log.contains("listening on"),
        "daemon startup lines missing: {log}"
    );
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
fn startup_failure_tail_shows_only_the_current_run() {
    let env = Env::new();
    let logs = env.root().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("chocofactoryd.log"),
        "=== choco server start earlier ===\nEARLIER-RUN-MARKER\n",
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let out = env.choco(&["server", "start", "--port", &port]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("failed to bind"), "{err}");
    assert!(!err.contains("EARLIER-RUN-MARKER"), "{err}");
}

#[test]
fn default_base_url_follows_the_lock_file() {
    let env = Env::new();
    let info = env.start();
    // Create a marker through the explicit URL, then check the no-flag
    // command sees it: only this HOME's daemon can know it.
    let repo = env.home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    env.json(&[
        "project",
        "create",
        "marker-project",
        "--repo",
        repo.to_str().unwrap(),
    ]);
    let out = env.choco(&["project", "list"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("marker-project"), "{}", text(&out));
    assert_ne!(info.port, 4141);
}

#[test]
fn stale_lock_makes_default_base_url_an_error_not_a_fallback() {
    let env = Env::new();
    let info = env.start();
    unsafe { libc::kill(info.pid as i32, libc::SIGKILL) };
    wait_for("the lock to be released", || {
        matches!(env.lock(), LockState::NotRunning { .. })
    });
    let out = env.choco(&["project", "list"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stdout(&out).is_empty(), "{}", text(&out));
    let err = stderr(&out);
    assert!(
        err.contains("not running") && err.contains(&info.pid.to_string()),
        "{err}"
    );
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

    // A refused stop also refuses the restart: same daemon, still answering.
    let before = env.running().pid;
    let out = env.choco(&["server", "restart"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert_eq!(env.running().pid, before);
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

/// A fake daemon. `/projects` answers with one project named `demo` and
/// `/tasks` with an empty list, both carrying the version header (when
/// given); with `fail_projects` `/projects` answers 500 instead.
async fn skew_server(version: Option<&'static str>, fail_projects: bool) -> String {
    use axum::{Router, routing::get};
    let headers = move || {
        let mut headers = axum::http::HeaderMap::new();
        if let Some(v) = version {
            headers.insert("x-chocofactory-version", v.parse().unwrap());
        }
        headers
    };
    let app = Router::new()
        .route(
            "/projects",
            get(move || async move {
                if fail_projects {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        headers(),
                        "{\"error\":\"boom\"}".to_string(),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        headers(),
                        r#"[{"id":"p1","name":"demo","repo_path":null,"created_at":"2026-01-01T00:00:00Z"}]"#
                            .to_string(),
                    )
                }
            }),
        )
        .route("/tasks", get(move || async move { (headers(), "[]") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn run_against(url: String, args: &'static [&'static str]) -> Output {
    tokio::task::spawn_blocking(move || {
        let env = Env::new();
        let mut a = vec!["--base-url", url.as_str(), "--json"];
        a.extend_from_slice(args);
        env.choco(&a)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn version_skew_warning_prints_once_across_several_requests() {
    // `task list --project demo` makes two requests: /projects then /tasks.
    let url = skew_server(Some("0.0.0-test"), false).await;
    let out = run_against(url, &["task", "list", "--project", "demo"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let err = stderr(&out);
    assert_eq!(err.matches("warning: choco").count(), 1, "{err}");
    assert!(err.contains("chocofactoryd 0.0.0-test"), "{err}");
    serde_json::from_slice::<Value>(&out.stdout).expect("stdout stays JSON");
}

#[tokio::test]
async fn version_skew_warning_fires_on_an_error_response() {
    let url = skew_server(Some("0.0.0-test"), true).await;
    let out = run_against(url, &["task", "list", "--project", "demo"]).await;
    assert!(!out.status.success(), "{}", text(&out));
    let err = stderr(&out);
    assert_eq!(err.matches("warning: choco").count(), 1, "{err}");
    assert!(err.contains("chocofactoryd 0.0.0-test"), "{err}");
}

#[tokio::test]
async fn missing_version_header_warns_once() {
    let url = skew_server(None, false).await;
    let out = run_against(url, &["task", "list", "--project", "demo"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let err = stderr(&out);
    assert_eq!(err.matches("doesn't report a version").count(), 1, "{err}");
    serde_json::from_slice::<Value>(&out.stdout).expect("stdout stays JSON");
}

#[test]
fn unanswering_daemon_is_reported_and_force_kills_it() {
    let env = Env::new();
    let info = env.start();
    let pid = info.pid as i32;
    unsafe { libc::kill(pid, libc::SIGSTOP) };

    let out = env.choco(&["server", "status"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("isn't answering"), "{}", text(&out));

    let out = env.choco(&["server", "stop"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("stops it anyway"), "{}", text(&out));
    assert_eq!(env.running().pid, info.pid);

    // SIGTERM stays pending on a stopped process, so only the SIGKILL
    // fallback after the stop timeout ends it.
    let out = env.choco(&["server", "stop", "--force"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stdout(&out).contains("was killed"), "{}", text(&out));
    wait_for("the lock to be released", || {
        matches!(env.lock(), LockState::NotRunning { .. })
    });
}

#[test]
fn start_times_out_without_killing_a_daemon_that_never_answers() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let dir = env.home.join("fake");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target_dir().join("choco"), dir.join("choco")).unwrap();
    let pidfile = dir.join("pid");
    let fake = dir.join("chocofactoryd");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ > {}\nexec sleep 120\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = env.choco_at(&dir.join("choco"), &["server", "start", "--port", "0"]);
    // The pid comes from the fake's own pidfile; this test started it.
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stderr(&out).contains("did not answer within 30s"),
        "{}",
        text(&out)
    );
    assert!(alive, "start must not kill the daemon it timed out on");
}

#[test]
fn start_keeps_waiting_through_an_unreadable_lock_and_reports_it() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root can open a mode-000 file; nothing to test
    }
    let env = Env::new();
    let dir = env.home.join("fake");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target_dir().join("choco"), dir.join("choco")).unwrap();
    let pidfile = dir.join("pid");
    let lockfile = env.root().join("chocofactoryd.lock");
    let fake = dir.join("chocofactoryd");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ > {}\n: > {lock}\nchmod 000 {lock}\nexec sleep 120\n",
            pidfile.display(),
            lock = lockfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let started = std::time::Instant::now();
    let out = env.choco_at(&dir.join("choco"), &["server", "start", "--port", "0"]);
    let elapsed = started.elapsed();
    // The pid comes from the fake's own pidfile; this test started it.
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = std::fs::set_permissions(&lockfile, std::fs::Permissions::from_mode(0o600));
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        elapsed >= std::time::Duration::from_secs(25),
        "start gave up after {elapsed:?} instead of waiting out the timeout"
    );
    let err = stderr(&out);
    assert!(err.contains("did not answer within 30s"), "{err}");
    assert!(err.contains("last lock error"), "{err}");
}

#[test]
fn start_reports_a_log_it_cannot_inspect() {
    let env = Env::new();
    let logs = env.root().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    // A self-referencing symlink: stat fails with ELOOP, not NotFound.
    std::os::unix::fs::symlink("chocofactoryd.log", logs.join("chocofactoryd.log")).unwrap();
    let out = env.choco(&["server", "start", "--port", "0"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("cannot inspect"), "{}", text(&out));
}

#[test]
fn restart_starts_a_daemon_even_when_stop_had_to_kill_the_old_one() {
    let env = Env::new();
    let info = env.start();
    unsafe { libc::kill(info.pid as i32, libc::SIGSTOP) };
    let out = env.choco(&["server", "restart", "--force"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stdout(&out).contains("was killed"), "{}", text(&out));
    assert!(stdout(&out).contains(" started (pid "), "{}", text(&out));
    let now = env.running();
    assert_ne!(now.pid, info.pid);
    assert_eq!(now.port, info.port);
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

#[test]
fn start_that_loses_the_race_reports_the_other_daemon() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;

    let env = Env::new();
    let dir = env.home.join("race");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target_dir().join("choco"), dir.join("choco")).unwrap();
    // A fake daemon that exits on its own, as one that lost the lock race would.
    let fake = dir.join("chocofactoryd");
    std::fs::write(&fake, "#!/bin/sh\nsleep 2\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let child = Command::new(dir.join("choco"))
        .args(["server", "start", "--port", "0"])
        .env("HOME", &env.home)
        .env("CHOCOFACTORY_CLAUDE_BINARY", &env.claude)
        .env_remove("CHOCO_BASE_URL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Once start has written its separator it has passed the "already running" check.
    let log = env.root().join("logs").join("chocofactoryd.log");
    let deadline = Instant::now() + DEADLINE;
    while !std::fs::read_to_string(&log)
        .map(|t| t.contains("=== choco server start"))
        .unwrap_or(false)
    {
        assert!(Instant::now() < deadline, "start never wrote its log");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Play the winning daemon: hold the lock and publish our own info.
    let me = std::process::id();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(env.root().join("chocofactoryd.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    file.set_len(0).unwrap();
    write!(
        file,
        r#"{{"pid":{me},"port":59999,"version":"0.0.0","commit":"c","started_at":"2026-01-01T00:00:00Z","exe":"/x"}}"#
    )
    .unwrap();
    file.sync_all().unwrap();

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let so = stdout(&out);
    assert!(
        so.contains("already running") && so.contains(&format!("pid {me}")),
        "{}",
        text(&out)
    );
}
