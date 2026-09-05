use std::sync::Arc;

use chocofactoryd::adapter::{AgentAdapter, ClaudeAdapter};
use chocofactoryd::api::{self, AppState};
use chocofactoryd::config_root;
use chocofactoryd::db::{self, task_runs};
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
    tracing_subscriber::fmt().with_env_filter(filter).init();
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

#[tokio::main]
async fn main() {
    init_logging();

    let root = config_root::config_root()
        .expect("chocofactoryd: $HOME is not set, cannot determine ~/.config/chocofactory");
    let workflows_dir = root.join("workflows");
    tracing::info!(root = %root.display(), "starting chocofactoryd");

    // Once before serving any request (P1-9): the built-ins ship compiled
    // into this binary and are seeded out to the user's own workflows
    // directory only if not already present (§2.2) — never overwritten on
    // a later version's startup.
    config_root::seed_builtin_workflows(&workflows_dir)
        .expect("chocofactoryd: failed to seed builtin workflow definitions");
    tracing::info!(dir = %workflows_dir.display(), "seeded builtin workflows");

    let db_path = root.join("chocofactory.db");
    let pool = db::connect(&db_path)
        .await
        .expect("chocofactoryd: failed to connect to the database");
    tracing::info!(path = %db_path.display(), "connected to database");

    // Before any SessionManager use (its own doc comment): any run left
    // `active` in the DB from a previous process is dead by now.
    let recovered = task_runs::recover_stale_active_runs(&pool)
        .await
        .expect("chocofactoryd: failed to recover stale active task runs");
    tracing::info!(recovered, "recovered stale active task runs");

    let events_notify = Arc::new(Notify::new());
    let mut claude_adapter = match claude_binary_override() {
        Some(binary) => ClaudeAdapter::with_binary(binary),
        None => ClaudeAdapter::new(),
    };
    if let Some(choco_binary) = choco_binary_override() {
        claude_adapter = claude_adapter.with_choco_binary(choco_binary);
    }
    let adapter: Arc<dyn AgentAdapter> = Arc::new(claude_adapter);
    let session_manager = SessionManager::new(
        pool.clone(),
        adapter,
        chrono::Duration::minutes(DEFAULT_IDLE_TIMEOUT_MINUTES),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new(
        pool.clone(),
        Arc::clone(&session_manager),
        workflows_dir,
        GlobalConfig::default_path(),
        Arc::clone(&events_notify),
    );

    tokio::spawn(Arc::clone(&session_manager).run_idle_reaper(IdleReaperConfig::default()));
    tokio::spawn(retention::run_retention_job(
        pool.clone(),
        RetentionConfig::default(),
    ));
    tracing::info!("spawned idle reaper and retention job");

    let state = AppState {
        pool,
        engine,
        events_notify,
    };
    let router = api::router(state).layer(TraceLayer::new_for_http());
    let port = port_override().unwrap_or(DEFAULT_PORT);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("chocofactoryd: failed to bind 127.0.0.1");
    tracing::info!(port, "listening on http://127.0.0.1:{port}");
    axum::serve(listener, router)
        .await
        .expect("chocofactoryd: server error");
}
