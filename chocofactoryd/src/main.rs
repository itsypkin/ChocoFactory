use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use chocofactory_core::daemon_lock::LockInfo;
use chocofactory_core::version;
use chocofactoryd::adapter::{ClaudeAdapter, OmpAdapter, Registry};
use chocofactoryd::api::{self, AppState, ExeStamp, ServerInfo};
use chocofactoryd::config_root;
use chocofactoryd::daemon_lock::DaemonLock;
use chocofactoryd::db::{self, sessions};
use chocofactoryd::engine::WorkflowEngine;
use chocofactoryd::global_config::GlobalConfig;
use chocofactoryd::retention::{self, RetentionConfig};
use chocofactoryd::session::{IdleReaperConfig, SessionManager};
use tokio::sync::Notify;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Every HTTP request/response (method, path, status, latency) logs at
/// `info` via `TraceLayer` regardless of `RUST_LOG`, so a plain `curl`
/// against the daemon is visible without extra setup; `RUST_LOG` still
/// overrides everything (e.g. `RUST_LOG=debug` for full detail, or
/// `RUST_LOG=chocofactoryd=trace,tower_http=debug` to narrow it down).
fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,chocofactoryd=debug,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Logs go to stderr, so colour is decided by whether *stderr* is a
        // terminal (tracing's default writer is stdout, which would make
        // that check meaningless).
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Bound to `127.0.0.1` only (design §6.1/§6.2, Q15: no auth, accessed
/// remotely only via SSH port forwarding).
const DEFAULT_PORT: u16 = 4141;

/// Overrides `DEFAULT_PORT` (§6.1's "no CLI flag/env var until something
/// downstream actually needs one" — the e2e test suite added in #42 is
/// that downstream need): lets it bind an ephemeral/test-only port
/// instead of colliding with a real `chocofactoryd` a developer might
/// already have running on 4141. Unset in normal use.
fn port_override() -> Option<u16> {
    std::env::var("CHOCOFACTORY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Overrides the adapter's `claude` binary path (#42) — set by the e2e
/// test suite and by manual smoke-testing to point at `mock-claude`
/// instead of the real, billable `claude` CLI. Unset in normal use, where
/// `ClaudeAdapter::new()`'s `"claude"` default applies unchanged.
fn claude_binary_override() -> Option<String> {
    std::env::var("CHOCOFACTORY_CLAUDE_BINARY").ok()
}

/// Overrides the adapter's `omp` binary path, mirroring
/// `CHOCOFACTORY_CLAUDE_BINARY`. Unset in normal use: `omp` from `PATH`.
fn omp_binary_override() -> Option<String> {
    std::env::var("CHOCOFACTORY_OMP_BINARY").ok()
}

/// Overrides the `choco` binary path the adapter embeds in every agent
/// turn's `--mcp-config` (issue #73). Unset in normal use, where
/// `ClaudeAdapter`'s sibling-of-`current_exe()` lookup applies unchanged;
/// set by the e2e test suite, which spawns `chocofactoryd` from a location
/// where that lookup wouldn't find the freshly built `choco` binary.
fn choco_binary_override() -> Option<String> {
    std::env::var("CHOCOFACTORY_CHOCO_BINARY").ok()
}

/// §4.1 leaves the idle-session timeout as "configurable, default TBD in
/// plan" — this is that default, hardcoded until a config surface for it
/// exists.
const DEFAULT_IDLE_TIMEOUT_MINUTES: i64 = 30;

/// How long in-flight HTTP requests get to finish once shutdown starts
/// before the server is abandoned (open WebSockets never finish on their
/// own).
const HTTP_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// How long `SessionManager::shutdown` waits for killed sessions to record
/// their end.
const SESSION_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Resolves once `rx` holds `true`.
async fn wait_for_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    // An `Err` means the sender is gone, which can only be the process
    // ending; treat it as shutdown too.
    let _ = rx.wait_for(|stop| *stop).await;
}

#[tokio::main]
async fn main() {
    // Before logging and before touching `$HOME`.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("chocofactoryd {}", version::long_version());
        return;
    }
    init_logging();

    let root = config_root::config_root()
        .expect("chocofactoryd: $HOME is not set, cannot determine ~/.config/chocofactory");
    std::fs::create_dir_all(&root).expect("chocofactoryd: failed to create the config directory");

    let mut claude_adapter = match claude_binary_override() {
        Some(binary) => ClaudeAdapter::with_binary(binary),
        None => ClaudeAdapter::new(),
    };
    if let Some(choco_binary) = choco_binary_override() {
        claude_adapter = claude_adapter.with_choco_binary(choco_binary);
    }
    let choco_binary = claude_adapter.choco_binary().to_string();
    // Always registered: an `omp` that isn't installed shows up as the
    // ordinary spawn error when a role on it takes a turn.
    let omp_state_dir = root.join("omp");
    let omp_adapter = match omp_binary_override() {
        Some(binary) => OmpAdapter::with_binary(binary, omp_state_dir),
        None => OmpAdapter::new(omp_state_dir),
    };
    let registry = Registry::new(vec![Arc::new(claude_adapter), Arc::new(omp_adapter)]);

    // Before the lock: a refused start leaves no lock, port or DB state.
    if let Err(err) = chocofactoryd::global_config::check_known_clis(
        GlobalConfig::default_path().as_deref(),
        &registry,
    ) {
        eprintln!("chocofactoryd: {err}");
        std::process::exit(1);
    }

    // The lock is the single-instance guard per config root; nothing below
    // (seeding, migrations, session recovery) may touch state before it is
    // held and the port is bound.
    let daemon_lock = match DaemonLock::acquire(&root) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("chocofactoryd: {err}");
            std::process::exit(1);
        }
    };

    // Installed right after the lock so a SIGTERM during startup is held
    // until serving begins and then shuts down gracefully, rather than
    // killing the process with sweep-spawned groups still running.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("chocofactoryd: failed to install the SIGTERM handler");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("chocofactoryd: failed to install the SIGINT handler");
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("SIGTERM received, shutting down"),
            _ = sigint.recv() => tracing::info!("SIGINT received, shutting down"),
        }
        let _ = stop_tx.send(true);
    });

    // Bind guards the port. The port is read back from the listener so
    // `CHOCOFACTORY_PORT=0` works.
    let requested_port = port_override().unwrap_or(DEFAULT_PORT);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", requested_port))
        .await
        .expect("chocofactoryd: failed to bind 127.0.0.1");
    let port = listener
        .local_addr()
        .expect("chocofactoryd: bound listener has no local address")
        .port();
    tracing::info!(port, "listening on http://127.0.0.1:{port}");

    let started_at = chrono::Utc::now();
    let exe = match ExeStamp::capture() {
        Ok(stamp) => Some(stamp),
        Err(err) => {
            tracing::warn!(%err, "could not stamp the daemon executable; exe_replaced will be unknown");
            None
        }
    };
    daemon_lock
        .publish(&LockInfo {
            pid: std::process::id(),
            port,
            version: version::VERSION.to_string(),
            commit: version::BUILD_COMMIT.map(str::to_string),
            started_at,
            exe: exe
                .as_ref()
                .map(|e| e.path.display().to_string())
                .unwrap_or_default(),
        })
        .expect("chocofactoryd: failed to write the lock file");

    tracing::info!(root = %root.display(), "starting chocofactoryd");

    // The built-ins ship compiled into this binary. Regenerated at every
    // start into a private read-only directory, before anything loads a
    // workflow (#129): the lock above makes this the only writer.
    let builtin_dir = root.join(".builtin-workflows");
    config_root::materialize_builtins(&builtin_dir)
        .expect("chocofactoryd: failed to write the built-in workflows");
    tracing::info!(dir = %builtin_dir.display(), "built-in workflows ready");

    let db_path = root.join("chocofactory.db");
    let pool = db::connect(&db_path)
        .await
        .expect("chocofactoryd: failed to connect to the database");
    tracing::info!(path = %db_path.display(), "connected to database");

    // Before any SessionManager use (its own doc comment): any session left
    // `active` in the DB from a previous process is dead by now.
    let recovered = sessions::recover_stale_active_sessions(&pool)
        .await
        .expect("chocofactoryd: failed to recover stale active sessions");
    tracing::info!(recovered, "recovered stale active sessions");

    report_legacy_workflows(&pool, &root.join("workflows")).await;

    let events_notify = Arc::new(Notify::new());
    let session_manager = SessionManager::new(
        pool.clone(),
        registry,
        chrono::Duration::minutes(DEFAULT_IDLE_TIMEOUT_MINUTES),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new(
        pool.clone(),
        Arc::clone(&session_manager),
        builtin_dir,
        GlobalConfig::default_path(),
        Arc::clone(&events_notify),
    )
    .with_legacy_workflows_dir(root.join("workflows"));

    tokio::spawn(Arc::clone(&session_manager).run_idle_reaper(IdleReaperConfig::default()));
    tokio::spawn(retention::run_retention_job(
        pool.clone(),
        RetentionConfig::default(),
    ));
    tracing::info!("spawned idle reaper and retention job");

    // First, and inline, before serving: an `agent_turn` or `shell` stage
    // whose process died with the previous daemon is parked `stuck`. Before
    // the poll sweep, because a resumed poll can advance into an
    // `agent_turn`, and that live turn must not be parked.
    let park = engine
        .park_interrupted_turns()
        .await
        .expect("chocofactoryd: failed to sweep interrupted agent and shell stages");
    tracing::info!(
        agent_turns = park.agent_turns,
        shells = park.shells,
        stuck_other = park.stuck_other,
        "parked interrupted agent and shell stages"
    );

    // After session recovery (a resumed poll may advance into an
    // `agent_turn`) and inline, before serving: `poll` stages whose runner
    // died with the previous process are re-entered with their stored
    // deadline (#52).
    let sweep = engine
        .resume_interrupted_polls()
        .await
        .expect("chocofactoryd: failed to sweep interrupted poll stages");
    tracing::info!(
        resumed = sweep.resumed,
        already_running = sweep.already_running,
        stuck = sweep.stuck,
        stage_kind_unrecorded = sweep.stage_kind_unrecorded,
        "resumed interrupted poll stages"
    );

    let state = AppState {
        pool,
        engine: engine.clone(),
        events_notify,
        server: Arc::new(ServerInfo {
            pid: std::process::id(),
            port,
            started_at,
            config_root: root.clone(),
            exe,
            choco_binary,
        }),
    };
    let router = api::router(state).layer(TraceLayer::new_for_http());

    let serve =
        axum::serve(listener, router).with_graceful_shutdown(wait_for_shutdown(stop_rx.clone()));
    let deadline = async {
        wait_for_shutdown(stop_rx).await;
        tokio::time::sleep(HTTP_DRAIN_GRACE).await;
    };
    tokio::select! {
        result = serve => result.expect("chocofactoryd: server error"),
        _ = deadline => tracing::warn!(
            "HTTP connections did not drain within {HTTP_DRAIN_GRACE:?}; continuing shutdown"
        ),
    }

    session_manager.shutdown(SESSION_DRAIN_GRACE).await;
    // Kill every shell/poll runner's process group *before* releasing the
    // lock, so a successor never overlaps with live runners of this daemon.
    engine.abort_all_detached_runners().await;
    drop(daemon_lock);
    tracing::info!("chocofactoryd stopped");
}

/// Reports on the old global workflows folder (#129), which is no longer
/// read. A report, not a precondition: a scan I/O error is logged at `error`
/// and startup continues, because nothing here changes what the daemon does.
async fn report_legacy_workflows(pool: &sqlx::SqlitePool, legacy_dir: &std::path::Path) {
    let scan = match config_root::scan_legacy_workflows(legacy_dir) {
        Ok(Some(scan)) => scan,
        Ok(None) => return,
        Err(err) => {
            tracing::error!(
                dir = %legacy_dir.display(), %err,
                "could not scan the old workflows folder; startup continues"
            );
            return;
        }
    };
    let dir = scan.dir.display();
    if scan.stale > 0 {
        tracing::info!(
            "ignoring {} stale copies of built-in workflows in {dir}: the folder is no longer \
             read (#129) and is safe to delete once no task uses it",
            scan.stale
        );
    }
    for path in scan.other.iter().filter(|p| !is_hidden_file(p)) {
        tracing::warn!(
            "{} differs from the current built-in (edited, or left over from an older version) \
             and is no longer read (#129). To keep using it, pass `choco task create --workflow \
             <path-to-its-workflow.yaml>` (prompts and scripts resolve next to it), or move it \
             with its prompts/ and scripts/ into a repo's .chocofactory/workflows/",
            path.display()
        );
    }
    let canonical = std::fs::canonicalize(&scan.dir).unwrap_or_else(|err| {
        tracing::error!(%err, "could not canonicalize the old workflows folder; matching tasks by its plain path");
        scan.dir.clone()
    });
    let prefix = legacy_prefix(&canonical);
    let mut in_use = tasks_using_prefix(pool, &prefix).await;
    // Pre-#88 tasks have no recorded path and still load `<folder>/<name>.yaml`.
    let legacy_defs = db::tasks::active_workflow_defs_without_path(pool)
        .await
        .expect("chocofactoryd: failed to list tasks without a recorded workflow path");
    in_use += legacy_defs
        .iter()
        .filter(|name| scan.dir.join(format!("{name}.yaml")).is_file())
        .count() as i64;
    if in_use > 0 {
        tracing::warn!(
            "{in_use} tasks still use workflows in {dir}; leave it in place until they finish"
        );
    }
}

/// Dotfiles such as `.DS_Store` are not workflows; don't warn about them.
fn is_hidden_file(path: &std::path::Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// The recorded-path prefix of tasks living in `folder`: the trailing `/`
/// keeps a sibling such as `workflows2/` from matching.
fn legacy_prefix(folder: &std::path::Path) -> String {
    format!("{}/", folder.display())
}

async fn tasks_using_prefix(pool: &sqlx::SqlitePool, prefix: &str) -> i64 {
    db::tasks::count_active_with_workflow_path_prefix(pool, prefix)
        .await
        .expect("chocofactoryd: failed to count tasks using the old workflows folder")
}

#[cfg(test)]
mod legacy_report_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn prefix_ends_with_a_slash_so_siblings_do_not_match() {
        let prefix = legacy_prefix(Path::new("/c/workflows"));
        assert_eq!(prefix, "/c/workflows/");
        assert!(!"/c/workflows2/x.yaml".starts_with(&prefix));
        assert!("/c/workflows/x.yaml".starts_with(&prefix));
    }

    #[test]
    fn dotfiles_are_hidden() {
        assert!(is_hidden_file(Path::new("/c/workflows/.DS_Store")));
        assert!(!is_hidden_file(Path::new("/c/workflows/chat.yaml")));
    }
}
