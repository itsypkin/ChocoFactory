//! `choco server start|stop|restart|status` (#84 part 2). These never use
//! `--base-url`: they read the daemon's lock file under `$HOME` and talk to
//! the port recorded there, so they can only ever reach this user's daemon.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use chocofactory_core::daemon_lock::{LockInfo, LockState, read_lock};
use chocofactory_core::models::ServerStatus;
use chocofactory_core::paths::config_root;
use chocofactory_core::version::{VERSION, long_version};
use serde_json::json;

use crate::cli::ServerCmd;
use crate::client::Client;
use crate::render;

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_millis(100);
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

const EXIT_NOT_RUNNING_OR_REFUSED: u8 = 3;

type Failure = String;

fn root() -> Result<PathBuf, Failure> {
    config_root().ok_or_else(|| "HOME is not set".to_string())
}

fn lock(root: &Path) -> Result<LockState, Failure> {
    read_lock(root).map_err(|e| format!("could not read the daemon lock file: {e}"))
}

fn log_path(root: &Path) -> PathBuf {
    root.join("logs").join("chocofactoryd.log")
}

fn url(info: &LockInfo) -> String {
    format!("http://127.0.0.1:{}", info.port)
}

fn skew_warning(info: &LockInfo) {
    if info.version != VERSION {
        eprintln!(
            "warning: choco {VERSION} is talking to chocofactoryd {}; run `choco server restart` (or `choco update`)",
            info.version
        );
    }
}

pub async fn run(cmd: ServerCmd, json: bool) -> ExitCode {
    let result = match cmd {
        ServerCmd::Start { port } => start(port).await,
        ServerCmd::Stop { force } => stop(force).await,
        ServerCmd::Restart { force, port } => restart(force, port).await,
        ServerCmd::Status => status(json).await,
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// The last `n` lines of the current run only: everything from the last
/// `=== choco server start` separator, so an earlier daemon's lines never show.
fn last_lines(path: &Path, n: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let from = text.rfind("=== choco server start").unwrap_or(0);
            let lines: Vec<&str> = text[from..].lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        Err(e) => format!("(could not read {}: {e})", path.display()),
    }
}

async fn start(port: Option<u16>) -> Result<u8, Failure> {
    let root = root()?;
    if let LockState::Running(info) = lock(&root)? {
        println!(
            "chocofactoryd is already running (pid {}, port {}, version {})",
            info.pid, info.port, info.version
        );
        skew_warning(&info);
        return Ok(0);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate choco itself: {e}"))?;
    let daemon = exe
        .parent()
        .ok_or("cannot locate choco's directory")?
        .join("chocofactoryd");
    if !daemon.is_file() {
        return Err(format!(
            "chocofactoryd not found next to choco (looked for {}); install them side by side",
            daemon.display()
        ));
    }
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
    let log = log_path(&root);
    let log_len = match std::fs::metadata(&log) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("cannot inspect {}: {e}", log.display())),
    };
    if log_len > LOG_ROTATE_BYTES {
        let rotated = logs.join("chocofactoryd.log.1");
        std::fs::rename(&log, &rotated).map_err(|e| {
            format!(
                "cannot rotate {} to {}: {e}",
                log.display(),
                rotated.display()
            )
        })?;
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    writeln!(
        file,
        "=== choco server start {} (choco {}) ===",
        chrono::Utc::now().to_rfc3339(),
        long_version()
    )
    .map_err(|e| format!("cannot write {}: {e}", log.display()))?;
    let err_file = file
        .try_clone()
        .map_err(|e| format!("cannot duplicate log handle: {e}"))?;

    let mut cmd = std::process::Command::new(&daemon);
    cmd.stdin(Stdio::null())
        .stdout(file)
        .stderr(err_file)
        .current_dir(std::env::var_os("HOME").ok_or("HOME is not set")?);
    if let Some(port) = port {
        cmd.env("CHOCOFACTORY_PORT", port.to_string());
    }
    // SAFETY: the closure only calls the async-signal-safe `setsid`.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", daemon.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_lock_error: Option<String> = None;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("cannot check on chocofactoryd (pid {pid}): {e}"))?
        {
            // A concurrent `start` may have won the race: if another daemon
            // now holds the lock, this one lost it and nothing is wrong.
            if let Ok(LockState::Running(info)) = lock(&root)
                && info.pid != pid
            {
                println!(
                    "chocofactoryd is already running (pid {}, port {}, version {})",
                    info.pid, info.port, info.version
                );
                skew_warning(&info);
                return Ok(0);
            }
            return Err(format!(
                "chocofactoryd exited during startup ({status}); last log lines:\n{}",
                last_lines(&log, 20)
            ));
        }
        // A transient lock-read error (the daemon is between taking the lock
        // and publishing it) means "not ready yet"; it is reported if the
        // deadline passes.
        let lock_state = lock(&root);
        if let Err(e) = &lock_state {
            last_lock_error = Some(e.clone());
        }
        if let Ok(LockState::Running(info)) = lock_state
            && info.pid == pid
        {
            let client = Client::new(url(&info));
            if let Ok(server) = client.server_status(PROBE_TIMEOUT).await
                && server.pid == pid
            {
                println!(
                    "chocofactoryd {} started (pid {pid}, port {}); log: {}",
                    server.version,
                    server.port,
                    log.display()
                );
                return Ok(0);
            }
        }
        if Instant::now() >= deadline {
            let lock_note = last_lock_error
                .map(|e| format!(" (last lock error: {e})"))
                .unwrap_or_default();
            return Err(format!(
                "chocofactoryd (pid {pid}) did not answer within {}s; it is still running; see {}{lock_note}",
                START_TIMEOUT.as_secs(),
                log.display()
            ));
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

fn signal(pid: u32, sig: libc::c_int) -> Result<(), Failure> {
    // SAFETY: plain kill(2) on a pid read from the daemon's lock file.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("cannot signal chocofactoryd (pid {pid}): {err}"));
        }
    }
    Ok(())
}

async fn wait_released(root: &Path, pid: u32, timeout: Duration) -> Result<bool, Failure> {
    let deadline = Instant::now() + timeout;
    loop {
        match lock(root)? {
            LockState::NotRunning { .. } => return Ok(true),
            LockState::Running(info) if info.pid != pid => return Ok(true),
            LockState::Running(_) => {}
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// Stops the daemon described by `info`. Exit code 0 stopped, 3 refused, 1
/// killed after the timeout.
async fn stop_running(root: &Path, info: LockInfo, force: bool) -> Result<u8, Failure> {
    let client = Client::new(url(&info));
    match client.server_status(STATUS_TIMEOUT).await {
        Ok(server) => {
            if !force && !server.in_flight.is_empty() {
                let mut text = String::from(
                    "error: chocofactoryd is in the middle of work that stopping would interrupt:\n",
                );
                for f in &server.in_flight {
                    text.push_str(&format!(
                        "  {}  {} ({})  {}\n",
                        f.task_id,
                        f.stage,
                        f.kind,
                        render::single_line(&f.title)
                    ));
                }
                text.push_str(
                    "Tasks waiting on a poll or a human are not affected. Wait for these to move on\n\
                     (`choco task status <id>`), or pass --force: they are marked stuck, and\n\
                     `choco task retry <id>` continues them (an agent turn resumes its session).",
                );
                println!("{text}");
                return Ok(EXIT_NOT_RUNNING_OR_REFUSED);
            }
        }
        Err(e) if !force => {
            return Err(format!(
                "chocofactoryd (pid {}) holds the lock but isn't answering on port {}: {e}; `choco server stop --force` stops it anyway",
                info.pid, info.port
            ));
        }
        Err(_) => {}
    }
    signal(info.pid, libc::SIGTERM)?;
    if wait_released(root, info.pid, STOP_TIMEOUT).await? {
        println!("chocofactoryd stopped (pid {})", info.pid);
        return Ok(0);
    }
    signal(info.pid, libc::SIGKILL)?;
    let released = wait_released(root, info.pid, Duration::from_secs(5)).await?;
    println!(
        "warning: chocofactoryd did not stop within {}s and was killed; agent processes it started may still be running; their tasks are marked stuck when it next starts",
        STOP_TIMEOUT.as_secs()
    );
    if !released {
        eprintln!(
            "error: chocofactoryd (pid {}) still holds the lock after SIGKILL",
            info.pid
        );
    }
    Ok(1)
}

async fn stop(force: bool) -> Result<u8, Failure> {
    let root = root()?;
    match lock(&root)? {
        LockState::NotRunning { .. } => {
            println!("chocofactoryd is not running");
            Ok(0)
        }
        LockState::Running(info) => stop_running(&root, info, force).await,
    }
}

async fn restart(force: bool, port: Option<u16>) -> Result<u8, Failure> {
    let root = root()?;
    match lock(&root)? {
        LockState::Running(info) => {
            let old_port = info.port;
            let code = stop_running(&root, info, force).await?;
            if code == EXIT_NOT_RUNNING_OR_REFUSED {
                return Ok(code);
            }
            // Code 1 means stop had to SIGKILL the daemon (the lock is released and a
            // warning was printed). The user still wants a daemon, so start one and
            // keep exit 1 as the warning.
            let started = start(port.or(Some(old_port))).await?;
            Ok(if code != 0 && started == 0 {
                code
            } else {
                started
            })
        }
        LockState::NotRunning { .. } => {
            println!("chocofactoryd was not running; starting it");
            start(port).await
        }
    }
}

async fn status(json: bool) -> Result<u8, Failure> {
    let root = root()?;
    let choco = json!({"version": VERSION, "commit": chocofactory_core::version::BUILD_COMMIT});
    let info = match lock(&root)? {
        LockState::NotRunning { last } => {
            if json {
                println!(
                    "{}",
                    json!({"running": false, "choco": choco, "daemon": null})
                );
            } else {
                match last {
                    Some(l) => println!("chocofactoryd is not running (last ran as pid {})", l.pid),
                    None => println!("chocofactoryd is not running"),
                }
            }
            return Ok(EXIT_NOT_RUNNING_OR_REFUSED);
        }
        LockState::Running(info) => info,
    };
    let log = log_path(&root);
    // The render below already warns about a version mismatch.
    let client = Client::new(url(&info));
    let client = if json {
        client
    } else {
        client.without_version_check()
    };
    let server: ServerStatus = client.server_status(STATUS_TIMEOUT).await.map_err(|e| {
        format!(
            "chocofactoryd (pid {}, port {}) holds the lock but isn't answering: {e}",
            info.pid, info.port
        )
    })?;
    if json {
        println!(
            "{}",
            json!({"running": true, "choco": choco, "daemon": server, "log": log.display().to_string()})
        );
    } else {
        println!(
            "{}",
            render::server_status(&server, &log, chrono::Utc::now())
        );
    }
    Ok(0)
}
