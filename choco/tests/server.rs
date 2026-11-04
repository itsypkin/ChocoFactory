//! `choco server start|stop|restart|status` against the real `choco` and the
//! real sibling `chocofactoryd` in `target/debug/` (#84 part 2).
//!
//! Safety rules, per test: its own temporary `HOME`, `--port 0`,
//! `CHOCOFACTORY_CLAUDE_BINARY` set to a fixture, `CHOCO_BASE_URL` removed.
//! Non-`server` commands only run after the temp HOME's lock shows a daemon
//! the test started. Daemons are signalled only by the pid in that lock, or by
//! `support::kill_processes_under` (processes whose cwd is inside the temp home).

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chocofactory_core::daemon_lock::{LockState, read_lock};
use chocofactory_test_support::wait_until_blocking_within;
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

    /// `choco [--json] <args>`: the command as a script runs it in each mode.
    fn act(&self, json: bool, args: &[&str]) -> Output {
        let mut a: Vec<&str> = Vec::new();
        if json {
            a.push("--json");
        }
        a.extend_from_slice(args);
        self.choco(&a)
    }

    /// The log `server start` reports, as it prints it.
    fn log_path(&self) -> PathBuf {
        self.root().join("logs").join("chocofactoryd.log")
    }

    /// The line `server start` prints for the daemon now holding the lock.
    fn started_line(&self, info: &chocofactory_core::daemon_lock::LockInfo) -> String {
        format!(
            "chocofactoryd {} started (pid {}, port {}); log: {}",
            info.version,
            info.pid,
            info.port,
            self.log_path().display()
        )
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
        support::kill_processes_under(&self.home);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Regression test for the leak: a daemon the lock file no longer names
/// (here: the lock file was deleted) must still die when the `Env` is
/// dropped by a panicking test.
#[test]
fn a_panicking_test_leaves_no_daemon_behind() {
    let seen: std::sync::Mutex<Option<(i32, PathBuf)>> = std::sync::Mutex::new(None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let env = Env::new();
        let info = env.start();
        *seen.lock().unwrap() = Some((info.pid as i32, std::fs::canonicalize(&env.home).unwrap()));
        // The state the lock-only `Drop` misses: `read_lock` reports
        // NotRunning while the daemon keeps running on the unlinked inode.
        std::fs::remove_file(chocofactory_core::daemon_lock::lock_path(&env.root())).unwrap();
        panic!("deliberate panic, as a failing test would");
    }));
    assert!(result.is_err());
    let (pid, home) = seen.lock().unwrap().take().unwrap();

    // The daemon is orphaned and pid 1 reaps it once killed, so waiting for
    // the pid to disappear is the thing under test, not a load allowance.
    let end = Instant::now() + DEADLINE;
    let gone = loop {
        let alive = unsafe { libc::kill(pid, 0) } != -1
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            break true;
        }
        if Instant::now() >= end {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !gone {
        // Don't leak when the fix is broken.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(gone, "daemon {pid} survived the Env being dropped");
    assert_eq!(support::kill_processes_under(&home), Vec::<i32>::new());
}

/// What `stop` prints after it had to SIGKILL the daemon.
const KILLED_WARNING: &str = "warning: chocofactoryd did not stop within 30s and was killed; agent processes it started may still be running; their tasks are marked stuck when it next starts";

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

/// The text an action printed. Under `--json` stdout must be empty and the
/// text is on stderr; otherwise it is on stdout and stderr is empty.
fn said(label: &str, out: &Output, json: bool) -> String {
    if json {
        support::assert_empty_stdout(label, out);
        stderr(out)
    } else {
        assert!(out.stderr.is_empty(), "{label}: {}", text(out));
        stdout(out)
    }
}

/// Where `server status --json` stands on "a turn is in flight", as a wait
/// predicate. A non-zero exit is "not yet": the daemon holds the lock but is
/// not answering (slow, or a failed read), so `choco` prints nothing on stdout
/// and the reason on stderr; for a dead daemon (exit 3) stdout says so. Both
/// streams are carried as `Err`. Exit 0 with
/// output that is not JSON is a broken `choco`: it panics with the parse error,
/// stdout and stderr, rather than letting the wait run to its bound.
fn in_flight_state(out: &Output) -> Result<(), String> {
    if !out.status.success() {
        return Err(format!(
            "`server status --json` exited with {}: stdout {:?}, stderr {:?}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`server status --json` exited 0 but printed output that is not JSON ({e}): stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    if v["daemon"]["in_flight"]
        .as_array()
        .is_none_or(|a| a.is_empty())
    {
        Err(format!("no turn in flight yet: {v}"))
    } else {
        Ok(())
    }
}

/// Waits up to `DEADLINE` for a predicate that says what it saw while not ready.
fn wait_for_state(what: &str, f: impl FnMut() -> Result<(), String>) {
    wait_until_blocking_within(DEADLINE, what, f);
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
    create_task_titled(env, workflow, "t")
}

fn create_task_titled(env: &Env, workflow: &str, title: &str) -> String {
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
        title,
        "--prompt",
        "go",
    ]);
    task["id"].as_str().unwrap().to_string()
}

fn wait_in_flight(env: &Env) {
    wait_for_state("an in-flight turn", || {
        let out = env.choco(&["--json", "server", "status"]);
        in_flight_state(&out)
    });
}

#[test]
fn version_flag_prints_without_a_daemon() {
    let expected = format!(
        "choco {} (dev build)\n",
        chocofactory_core::version::VERSION
    );
    for (flag, unreachable) in [("--version", false), ("-V", true)] {
        let env = Env::new();
        let bin = target_dir().join("choco");
        let mut cmd = Command::new(&bin);
        cmd.arg(flag)
            .env("HOME", &env.home)
            .env("RUST_LOG", "error")
            .env_remove("CHOCO_BASE_URL");
        if unreachable {
            cmd.env("CHOCO_BASE_URL", "http://127.0.0.1:9");
        }
        let out = cmd.output().expect("failed to run choco");
        assert!(out.status.success(), "{flag}: {}", text(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{flag}");
        assert!(out.stderr.is_empty(), "{flag}: {}", text(&out));
        assert!(!env.home.join(".config").exists(), "{flag} touched $HOME");
    }
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
    port_taken_scenario(false);
}

#[test]
fn start_fails_when_port_is_taken_json() {
    port_taken_scenario(true);
}

fn port_taken_scenario(json: bool) {
    let env = Env::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let began = Instant::now();
    let out = env.act(json, &["server", "start", "--port", &port]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    support::assert_empty_stdout("server start (port taken)", &out);
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
    wait_for_state("the lock to be released", || match env.lock() {
        LockState::NotRunning { .. } => Ok(()),
        other => Err(format!("lock state {other:?}")),
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
    wait_for_state("the task to be stuck", || {
        let detail = env.json(&["task", "status", &id]);
        if detail["status"] == "stuck" || detail["task"]["status"] == "stuck" {
            Ok(())
        } else {
            Err(format!("task status {detail}"))
        }
    });
}

/// A turn waiting for an agent slot has no process: it is not in flight, so
/// it never blocks `stop`/`restart`, the CLI says where it will be, and after
/// the restart it starts in its place (it is not parked `stuck`).
#[test]
fn queued_turns_do_not_block_stop_and_start_after_the_restart() {
    queued_turns_scenario(false);
}

#[test]
fn queued_turns_do_not_block_stop_and_start_after_the_restart_json() {
    queued_turns_scenario(true);
}

fn queued_turns_scenario(json: bool) {
    let probe = Env::new();
    let claude = blocking_claude(&probe.home);
    let env = Env {
        home: probe.home.clone(),
        claude,
    };
    std::mem::forget(probe); // `env` owns the same directory now
    std::fs::create_dir_all(env.root()).unwrap();
    std::fs::write(env.root().join("config.yaml"), "max_agents: 1\n").unwrap();
    env.start();
    let wf = env.home.join("wf.yaml");
    std::fs::write(&wf, AGENT_WF).unwrap();
    let repo = env.home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    env.json(&[
        "project",
        "create",
        "demo",
        "--repo",
        repo.to_str().unwrap(),
    ]);
    let create = |title: &str| -> String {
        let task = env.json(&[
            "task",
            "create",
            "--project",
            "demo",
            "--workflow",
            wf.to_str().unwrap(),
            "--title",
            title,
            "--prompt",
            "go",
        ]);
        task["id"].as_str().unwrap().to_string()
    };
    let first = create("first");
    wait_in_flight(&env);
    let second = create("second");

    // The first is running; the second is waiting, with its place in line.
    let detail = env.json(&["task", "status", &second]);
    assert_eq!(detail["status"], "open", "{detail}");
    assert_eq!(detail["queued"][0]["position"], 1, "{detail}");
    assert_eq!(detail["queued"][0]["pool"], "server", "{detail}");
    let status = env.choco(&["server", "status"]);
    let all = stdout(&status);
    assert!(all.contains("server 1/1 running, 1 queued"), "{all}");
    assert!(all.contains("queued") && all.contains(&second), "{all}");

    // Stop refuses for the running turn only, and says what happens to the
    // queued one.
    let out = env.act(json, &["server", "stop"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let all = said("server stop (refused, queued)", &out, json);
    assert!(all.contains(&first) && !all.contains(&second), "{all}");
    assert!(
        all.contains("1 queued turn will wait and start in order after the restart."),
        "{all}"
    );

    let stopped_pid = env.running().pid;
    let out = env.act(json, &["server", "stop", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        said("server stop --force (queued note)", &out, json),
        format!(
            "1 queued turn will wait and start in order after the restart.\nchocofactoryd stopped (pid {stopped_pid})\n"
        )
    );
    env.start();
    // The running turn was parked; the queued one was not, and now runs.
    wait_for_state("the first task to be stuck", || {
        let detail = env.json(&["task", "status", &first]);
        if detail["status"] == "stuck" {
            Ok(())
        } else {
            Err(format!("task status {detail}"))
        }
    });
    wait_for_state("the queued turn to start", || {
        let detail = env.json(&["task", "status", &second]);
        let server = env.json(&["server", "status"]);
        let started = detail["status"] == "open"
            && detail["queued"].as_array().is_some_and(|q| q.is_empty())
            && server["daemon"]["in_flight"]
                .as_array()
                .is_some_and(|a| a.iter().any(|f| f["task_id"] == second.as_str()));
        if started {
            Ok(())
        } else {
            Err(format!("task {detail}; server {server}"))
        }
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
    wait_for_state("the lock to be released", || match env.lock() {
        LockState::NotRunning { .. } => Ok(()),
        other => Err(format!("lock state {other:?}")),
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
    unanswering_scenario(false);
}

#[test]
fn unanswering_daemon_is_reported_and_force_kills_it_json() {
    unanswering_scenario(true);
}

fn unanswering_scenario(json: bool) {
    let env = Env::new();
    let info = env.start();
    let pid = info.pid as i32;
    unsafe { libc::kill(pid, libc::SIGSTOP) };

    let out = env.act(json, &["server", "status"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("isn't answering"), "{}", text(&out));

    let out = env.act(json, &["server", "stop"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    support::assert_empty_stdout("server stop (unanswering)", &out);
    assert!(stderr(&out).contains("stops it anyway"), "{}", text(&out));
    assert_eq!(env.running().pid, info.pid);

    // SIGTERM stays pending on a stopped process, so only the SIGKILL
    // fallback after the stop timeout ends it.
    let out = env.act(json, &["server", "stop", "--force"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(
        said("server stop --force (killed)", &out, json),
        format!("{KILLED_WARNING}\n")
    );
    wait_for_state("the lock to be released", || match env.lock() {
        LockState::NotRunning { .. } => Ok(()),
        other => Err(format!("lock state {other:?}")),
    });
}

#[test]
fn start_times_out_without_killing_a_daemon_that_never_answers() {
    start_times_out_scenario(false);
}

#[test]
fn start_times_out_without_killing_a_daemon_that_never_answers_json() {
    start_times_out_scenario(true);
}

fn start_times_out_scenario(json: bool) {
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

    let mut args = vec!["server", "start", "--port", "0"];
    if json {
        args.insert(0, "--json");
    }
    let out = env.choco_at(&dir.join("choco"), &args);
    // The pid comes from the fake's own pidfile; this test started it.
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    support::assert_empty_stdout("server start (timeout)", &out);
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
    restart_after_kill_scenario(false);
}

#[test]
fn restart_starts_a_daemon_even_when_stop_had_to_kill_the_old_one_json() {
    restart_after_kill_scenario(true);
}

fn restart_after_kill_scenario(json: bool) {
    let env = Env::new();
    let info = env.start();
    unsafe { libc::kill(info.pid as i32, libc::SIGSTOP) };
    let out = env.act(json, &["server", "restart", "--force"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let now = env.running();
    assert_eq!(
        said("server restart --force (killed)", &out, json),
        format!("{KILLED_WARNING}\n{}\n", env.started_line(&now))
    );
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
    no_sibling_scenario(false);
}

#[test]
fn start_without_a_sibling_daemon_fails_json() {
    no_sibling_scenario(true);
}

fn no_sibling_scenario(json: bool) {
    let env = Env::new();
    let lone = env.home.join("lone");
    std::fs::create_dir_all(&lone).unwrap();
    std::fs::copy(target_dir().join("choco"), lone.join("choco")).unwrap();
    let mut args = vec!["server", "start", "--port", "0"];
    if json {
        args.insert(0, "--json");
    }
    let out = env.choco_at(&lone.join("choco"), &args);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    support::assert_empty_stdout("server start (no sibling)", &out);
    assert!(
        stderr(&out).contains("chocofactoryd not found next to choco"),
        "{}",
        text(&out)
    );
}

#[test]
fn start_that_loses_the_race_reports_the_other_daemon() {
    race_scenario(false);
}

#[test]
fn start_that_loses_the_race_reports_the_other_daemon_json() {
    race_scenario(true);
}

fn race_scenario(json: bool) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;

    let env = Env::new();
    let dir = env.home.join("race");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target_dir().join("choco"), dir.join("choco")).unwrap();
    // A fake daemon that exits as one that lost the lock race would, but only
    // once the test has published the winner's lock (no sleep to race against).
    let fake = dir.join("chocofactoryd");
    let go = dir.join("go");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nuntil [ -e '{}' ]; do sleep 0.05; done\nexit 1\n",
            go.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut args = vec!["server", "start", "--port", "0"];
    if json {
        args.insert(0, "--json");
    }
    let child = Command::new(dir.join("choco"))
        .args(&args)
        .env("HOME", &env.home)
        .env("CHOCOFACTORY_CLAUDE_BINARY", &env.claude)
        .env_remove("CHOCO_BASE_URL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Once start has written its separator it has passed the "already running" check.
    let log = env.root().join("logs").join("chocofactoryd.log");
    wait_until_blocking_within(DEADLINE, "start to write its separator to the log", || {
        match std::fs::read_to_string(&log) {
            Ok(t) if t.contains("=== choco server start") => Ok(()),
            Ok(t) => Err(format!("log so far: {t:?}")),
            // Not created yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err("no log file yet".to_string())
            }
            Err(e) => panic!("reading {}: {e}", log.display()),
        }
    });

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
    std::fs::write(&go, "").unwrap();

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let running = format!("chocofactoryd is already running (pid {me}, port 59999, version 0.0.0)");
    let skew = format!(
        "warning: choco {} is talking to chocofactoryd 0.0.0; run `choco server restart` (or `choco update`)",
        chocofactory_core::version::VERSION
    );
    // The warning is always stderr; the message follows the mode.
    if json {
        support::assert_empty_stdout("server start (lost the race)", &out);
        assert_eq!(stderr(&out), format!("{running}\n{skew}\n"));
    } else {
        assert_eq!(stdout(&out), format!("{running}\n"));
        assert_eq!(stderr(&out), format!("{skew}\n"));
    }
}

#[test]
fn restart_does_not_start_when_the_killed_daemon_still_holds_the_lock() {
    unreleased_lock_scenario(false);
}

#[test]
fn restart_does_not_start_when_the_killed_daemon_still_holds_the_lock_json() {
    unreleased_lock_scenario(true);
}

fn unreleased_lock_scenario(json: bool) {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let env = Env::new();
    // The test's own child stands in for the daemon; the test holds the lock,
    // so SIGKILL on the child can never release it.
    let mut victim = Command::new("sleep").arg("300").spawn().unwrap();
    std::fs::create_dir_all(env.root()).unwrap();
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
        r#"{{"pid":{},"port":59998,"version":"0.0.0","commit":"c","started_at":"2026-01-01T00:00:00Z","exe":"/x"}}"#,
        victim.id()
    )
    .unwrap();
    file.sync_all().unwrap();

    let out = env.act(json, &["server", "restart", "--force"]);
    let _ = victim.kill();
    let _ = victim.wait();

    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    if json {
        support::assert_empty_stdout("server restart --force (unreleased lock)", &out);
        assert!(stderr(&out).contains(KILLED_WARNING), "{}", text(&out));
    }
    assert!(
        stderr(&out).contains("still holds the lock after SIGKILL"),
        "{}",
        text(&out)
    );
    assert!(!stdout(&out).contains(" started (pid "), "{}", text(&out));
    // Calling `start` anyway would only report the old daemon as running.
    assert!(!text(&out).contains("already running"), "{}", text(&out));
    let log = env.root().join("logs").join("chocofactoryd.log");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !logged.contains("=== choco server start"),
        "no new daemon should have been started: {logged}"
    );
    drop(file);
}

async fn status_server() -> String {
    use axum::{Router, routing::get};
    let app = Router::new().route(
        "/server",
        get(|| async {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("x-chocofactory-version", "0.0.0-test".parse().unwrap());
            let body = serde_json::json!({
                "version": "0.0.0-test", "commit": null, "pid": std::process::id(),
                "port": 1, "started_at": "2026-01-01T00:00:00Z", "config_root": "/x",
                "exe": "/x", "exe_replaced": false, "choco_binary": "/x",
                "choco_binary_found": true, "tasks": {}, "in_flight": []
            });
            (headers, body.to_string())
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port.to_string()
}

#[tokio::test]
async fn status_warns_once_for_a_version_mismatch_in_json_and_human_modes() {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let port = status_server().await;
    let (json_out, human_out) = tokio::task::spawn_blocking(move || {
        let env = Env::new();
        std::fs::create_dir_all(env.root()).unwrap();
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
        write!(
            file,
            r#"{{"pid":{},"port":{port},"version":"0.0.0-test","commit":"c","started_at":"2026-01-01T00:00:00Z","exe":"/x"}}"#,
            std::process::id()
        )
        .unwrap();
        file.sync_all().unwrap();
        let j = env.choco(&["--json", "server", "status"]);
        let h = env.choco(&["server", "status"]);
        drop(file);
        (j, h)
    })
    .await
    .unwrap();

    // --json: the client's own warning, once, on stderr; stdout stays JSON.
    assert!(json_out.status.success(), "{}", text(&json_out));
    let err = stderr(&json_out);
    assert_eq!(err.matches("warning: choco").count(), 1, "{err}");
    serde_json::from_slice::<Value>(&json_out.stdout).expect("stdout stays JSON");

    // Human: the render prints the warning (stdout); the client stays quiet.
    assert!(human_out.status.success(), "{}", text(&human_out));
    assert!(
        !stderr(&human_out).contains("warning:"),
        "{}",
        text(&human_out)
    );
    assert_eq!(
        stdout(&human_out).matches("warning: choco").count(),
        1,
        "{}",
        text(&human_out)
    );
}

fn status_output(code: i32, stdout: &str, stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt;
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn a_status_call_that_exits_non_zero_is_not_yet_and_carries_its_stderr() {
    // What `choco --json server status` does when the daemon holds the lock
    // but is not answering: empty stdout, the reason on stderr, exit 1.
    let err = in_flight_state(&status_output(
        1,
        "",
        "error: chocofactoryd (pid 1, port 1) holds the lock but isn't answering: failed to connect",
    ))
    .unwrap_err();
    assert!(
        err.contains("exited with") && err.contains("isn't answering: failed to connect"),
        "{err}"
    );
}

#[test]
fn a_status_call_for_a_dead_daemon_is_not_yet_and_carries_its_stdout() {
    // `choco --json server status` on a daemon that is not running exits 3 and
    // says so on stdout, with nothing on stderr.
    let err =
        in_flight_state(&status_output(3, r#"{"running":false,"daemon":null}"#, "")).unwrap_err();
    assert!(
        err.contains("exited with") && err.contains("running"),
        "{err}"
    );
}

#[test]
fn unparseable_server_status_output_fails_the_wait_at_once_naming_the_parse_error() {
    assert!(
        in_flight_state(&status_output(
            0,
            r#"{"daemon":{"in_flight":[{"task_id":"t"}]}}"#,
            ""
        ))
        .is_ok()
    );
    assert!(in_flight_state(&status_output(0, r#"{"daemon":{"in_flight":[]}}"#, "")).is_err());
    let err = std::panic::catch_unwind(|| {
        let _ = in_flight_state(&status_output(0, "starting up...", "a warning"));
    })
    .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap();
    assert!(
        msg.contains("not JSON")
            && msg.contains("expected")
            && msg.contains("starting up...")
            && msg.contains("a warning"),
        "{msg}"
    );
}

// ---------------------------------------------------------------------------
// #361: under `--json`, stdout carries JSON or nothing. `server start|stop|
// restart` have no JSON result, so their messages go to stderr, word for
// word, and the exit codes are unchanged.
// ---------------------------------------------------------------------------

fn lifecycle_scenario(json: bool) {
    let env = Env::new();
    let say = |label: &str, out: &Output| said(label, out, json);

    // restart with nothing running: the note, then the start line.
    let out = env.act(json, &["server", "restart", "--port", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let first = env.running();
    assert_eq!(
        say("server restart (not running)", &out),
        format!(
            "chocofactoryd was not running; starting it\n{}\n",
            env.started_line(&first)
        )
    );

    // start while running.
    let out = env.act(json, &["server", "start"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        say("server start (running)", &out),
        format!(
            "chocofactoryd is already running (pid {}, port {}, version {})\n",
            first.pid, first.port, first.version
        )
    );

    // restart while running: stopped, then started on the same port.
    let out = env.act(json, &["server", "restart"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let second = env.running();
    assert_ne!(second.pid, first.pid);
    assert_eq!(second.port, first.port);
    assert_eq!(
        say("server restart (running)", &out),
        format!(
            "chocofactoryd stopped (pid {})\n{}\n",
            first.pid,
            env.started_line(&second)
        )
    );

    // stop, then stop again.
    let out = env.act(json, &["server", "stop"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        say("server stop (running)", &out),
        format!("chocofactoryd stopped (pid {})\n", second.pid)
    );
    let out = env.act(json, &["server", "stop"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        say("server stop (not running)", &out),
        "chocofactoryd is not running\n"
    );

    // start from stopped.
    let out = env.act(json, &["server", "start", "--port", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let third = env.running();
    assert_eq!(
        say("server start (stopped)", &out),
        format!("{}\n", env.started_line(&third))
    );
}

#[test]
fn server_actions_print_their_messages_on_stdout() {
    lifecycle_scenario(false);
}

#[test]
fn server_actions_print_nothing_on_stdout_under_json() {
    lifecycle_scenario(true);
}

const HELD_SHELL_WF: &str = r#"
name: held
stages:
  run:
    kind: shell
    command: "sleep 600"
    on: { done: finished }
  finished:
    kind: terminal
"#;

const CTL_TITLE: &str = "title \u{1b}[31mred \u{1b}[0m \u{1}\u{1f} x \u{7f}";

fn cancel_task(env: &Env, id: &str) {
    let base = format!("http://127.0.0.1:{}", env.running().port);
    let out = env.choco(&["--json", "--base-url", &base, "task", "cancel", id]);
    assert!(out.status.success(), "cancel: {}", text(&out));
    wait_for_state("the cancelled task to leave the in-flight list", || {
        let out = env.choco(&["--json", "server", "status"]);
        let v = support::json_stdout("server status", &out);
        if v["daemon"]["in_flight"].as_array().is_some_and(|a| a.is_empty()) {
            Ok(())
        } else {
            Err(format!("still in flight: {v}"))
        }
    });
}

/// A real shell stage is in flight: stop and restart refuse (exit 3) without
/// touching the daemon, in both modes, and the refusal text is the same
/// bytes on whichever stream the mode picks.
#[test]
fn refusal_goes_to_stderr_under_json_and_leaves_the_daemon_running() {
    let env = Env::new();
    let info = env.start();
    let id = create_task(&env, HELD_SHELL_WF);
    wait_in_flight(&env);

    let mut texts: Vec<(String, String)> = Vec::new();
    for sub in ["stop", "restart"] {
        let human = env.act(false, &["server", sub]);
        let json = env.act(true, &["server", sub]);
        for (out, mode) in [(&human, false), (&json, true)] {
            let label = format!("server {sub} (refused, json={mode})");
            assert_eq!(out.status.code(), Some(3), "{label}: {}", text(out));
            assert!(stderr(out).contains(&id) || !mode, "{label}: {}", text(out));
        }
        let h = said("server stop/restart (human)", &human, false);
        let j = said("server stop/restart (json)", &json, true);
        assert!(h.contains(&id) && h.contains("(shell)"), "{h}");
        assert_eq!(h, j, "the refusal text differs between modes");
        texts.push((sub.to_string(), j));
        assert_eq!(env.running().pid, info.pid, "{sub} touched the daemon");
    }
    // `stop` and `restart` share the refusal.
    assert_eq!(texts[0].1, texts[1].1);
    cancel_task(&env, &id);
}

#[test]
fn server_status_json_keeps_a_control_byte_title_intact() {
    let env = Env::new();
    env.start();
    let id = create_task_titled(&env, HELD_SHELL_WF, CTL_TITLE);
    wait_in_flight(&env);
    let out = env.choco(&["--json", "server", "status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let v = support::json_stdout("server status", &out);
    assert_eq!(v["running"], true);
    assert_eq!(v["daemon"]["in_flight"][0]["task_id"], id.as_str());
    assert_eq!(v["daemon"]["in_flight"][0]["title"], CTL_TITLE);
    cancel_task(&env, &id);
}

/// Kills a spawned process on drop, so a failed assertion leaks no child.
struct KillOnDrop(Option<std::process::Child>);

impl KillOnDrop {
    fn new(child: std::process::Child) -> Self {
        KillOnDrop(Some(child))
    }

    fn id(&self) -> u32 {
        self.0.as_ref().expect("child already finished").id()
    }

    /// Waits for the child to end and returns what it wrote.
    fn finish(mut self) -> Output {
        self.0
            .take()
            .expect("child already finished")
            .wait_with_output()
            .unwrap()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn signal_exit(out: &Output) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    out.status.signal()
}

fn interrupted_start_scenario(json: bool) {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let dir = env.home.join("fake");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target_dir().join("choco"), dir.join("choco")).unwrap();
    let pidfile = dir.join("pid");
    let fake = dir.join("chocofactoryd");
    // Started, never ready: no lock, no answer.
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho $$ > {}\nexec sleep 120\n", pidfile.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut args = vec!["server", "start", "--port", "0"];
    if json {
        args.insert(0, "--json");
    }
    let child = Command::new(dir.join("choco"))
        .args(&args)
        .env("HOME", &env.home)
        .env("RUST_LOG", "error")
        .env_remove("CHOCO_BASE_URL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let cli_pid = child.id() as i32;
    let guard = KillOnDrop::new(child);
    let daemon_pid: i32 = {
        let mut seen = None;
        wait_for_state("the fake daemon to start", || {
            match std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                Some(p) => {
                    seen = Some(p);
                    Ok(())
                }
                None => Err("no pid yet".to_string()),
            }
        });
        seen.unwrap()
    };
    assert_eq!(unsafe { libc::kill(cli_pid, libc::SIGINT) }, 0);
    let out = guard.finish();
    // Killed by SIGINT, not a clean exit.
    assert_eq!(signal_exit(&out), Some(libc::SIGINT), "{}", text(&out));
    support::assert_empty_stdout("server start (interrupted)", &out);
    // The detached daemon and the log stay behind, as before; no lock was taken.
    let alive = unsafe { libc::kill(daemon_pid, 0) } == 0;
    unsafe { libc::kill(daemon_pid, libc::SIGKILL) };
    assert!(alive, "interrupting start must not kill the daemon it spawned");
    assert!(matches!(env.lock(), LockState::NotRunning { .. }));
    let log = std::fs::read_to_string(env.log_path()).unwrap();
    assert!(log.contains("=== choco server start"), "{log}");
}

#[test]
fn interrupting_start_leaves_the_same_state_in_both_modes() {
    interrupted_start_scenario(false);
}

#[test]
fn interrupting_start_leaves_the_same_state_under_json() {
    interrupted_start_scenario(true);
}

async fn interrupted_stop_scenario(json: bool) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    let port = status_server().await;
    let env = Env::new();
    let receipt = env.home.join("got-sigterm");
    let ready = env.home.join("handler-installed");
    let script = env.home.join("owner.sh");
    // A daemon stand-in that notes SIGTERM and keeps running.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap 'echo got > {}' TERM\necho ready > {}\nwhile :; do sleep 0.1; done\n",
            receipt.display(),
            ready.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let owner = KillOnDrop::new(
        Command::new(&script)
            .current_dir(&env.home)
            .spawn()
            .unwrap(),
    );
    let owner_pid = owner.id();
    // Only after the trap exists does SIGTERM mean "noted" rather than "dead".
    wait_for_state("the owner to install its SIGTERM handler", || {
        if ready.exists() {
            Ok(())
        } else {
            Err("handler not installed yet".to_string())
        }
    });
    std::fs::create_dir_all(env.root()).unwrap();
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
    write!(
        file,
        r#"{{"pid":{owner_pid},"port":{port},"version":"0.0.0-test","commit":"c","started_at":"2026-01-01T00:00:00Z","exe":"/x"}}"#,
    )
    .unwrap();
    file.sync_all().unwrap();

    let mut args = vec!["server", "stop"];
    if json {
        args.insert(0, "--json");
    }
    let cli = KillOnDrop::new(
        Command::new(target_dir().join("choco"))
            .args(&args)
            .env("HOME", &env.home)
            .env("RUST_LOG", "error")
            .env_remove("CHOCO_BASE_URL")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let cli_pid = cli.id() as i32;
    wait_for_state("the owner to receive SIGTERM", || {
        if receipt.exists() {
            Ok(())
        } else {
            Err("no SIGTERM yet".to_string())
        }
    });
    assert_eq!(unsafe { libc::kill(cli_pid, libc::SIGINT) }, 0);
    let out = cli.finish();
    assert_eq!(signal_exit(&out), Some(libc::SIGINT), "{}", text(&out));
    support::assert_empty_stdout("server stop (interrupted)", &out);
    // The same owner still holds the lock, and was not SIGKILLed.
    match env.lock() {
        LockState::Running(info) => assert_eq!(info.pid, owner_pid),
        other => panic!("lock should still be held: {other:?}"),
    }
    assert_eq!(unsafe { libc::kill(owner_pid as i32, 0) }, 0);
    drop(file);
    drop(owner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_stop_leaves_the_owner_in_place() {
    interrupted_stop_scenario(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_stop_leaves_the_owner_in_place_under_json() {
    interrupted_stop_scenario(true).await;
}
