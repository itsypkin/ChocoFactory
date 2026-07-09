use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chokofactory_core::models::TaskRunStatus;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, mpsc};

use crate::adapter::{AdapterError, AgentAdapter, AgentEvent, AgentHandle, RoleConfig};
use crate::db::{events, task_runs};

/// Drives the active ⇄ idle ⇄ resume state machine (§4.1) on top of
/// `task_runs`: keeps a live `AgentHandle` per active `task_run_id`,
/// drains its events into the `events` table, and resumes a fresh
/// process from the persisted `session_id` when a message arrives for a
/// run that isn't currently live in memory.
pub struct SessionManager {
    pool: SqlitePool,
    adapter: Arc<dyn AgentAdapter>,
    idle_timeout: chrono::Duration,
    sessions: Mutex<HashMap<String, ActiveSession>>,
}

struct ActiveSession {
    cmd_tx: mpsc::UnboundedSender<Command>,
    last_activity: Arc<Mutex<DateTime<Utc>>>,
}

enum Command {
    Send(String),
    Close,
}

#[derive(Debug)]
pub enum SessionError {
    UnknownTaskRun,
    NotResumable(TaskRunStatus),
    Adapter(AdapterError),
    Db(sqlx::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::UnknownTaskRun => write!(f, "no such task run"),
            SessionError::NotResumable(status) => {
                write!(f, "task run is {status} and has no session to resume")
            }
            SessionError::Adapter(err) => write!(f, "{err}"),
            SessionError::Db(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Config for the background idle reaper (§4.3).
#[derive(Debug, Clone)]
pub struct IdleReaperConfig {
    pub interval: Duration,
}

impl Default for IdleReaperConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
        }
    }
}

impl SessionManager {
    pub fn new(
        pool: SqlitePool,
        adapter: Arc<dyn AgentAdapter>,
        idle_timeout: chrono::Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            adapter,
            idle_timeout,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Starts a brand-new subprocess for `task_run_id` and begins
    /// draining its events (§4.1 step 1). The caller is responsible for
    /// having already created the `task_runs` row (it's created `active`
    /// by `task_runs::create`).
    pub async fn start(
        self: &Arc<Self>,
        task_run_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<(), SessionError> {
        let handle = self
            .adapter
            .start(prompt, cfg)
            .map_err(SessionError::Adapter)?;
        self.spawn_drain(task_run_id.to_string(), handle).await;
        Ok(())
    }

    /// Sends a message to `task_run_id`. If the run has a live subprocess
    /// in memory, forwards straight to its stdin. Otherwise resumes a
    /// fresh process from the persisted `session_id` (§4.1 step 3) and
    /// flips the run back to `active`.
    pub async fn send_message(
        self: &Arc<Self>,
        task_run_id: &str,
        text: &str,
        cfg: &RoleConfig,
    ) -> Result<(), SessionError> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(task_run_id) {
                *session.last_activity.lock().await = Utc::now();
                session
                    .cmd_tx
                    .send(Command::Send(text.to_string()))
                    .map_err(|_| SessionError::UnknownTaskRun)?;
                return Ok(());
            }
        }

        let task_run = task_runs::get(&self.pool, task_run_id)
            .await
            .map_err(SessionError::Db)?
            .ok_or(SessionError::UnknownTaskRun)?;
        let Some(session_id) = task_run.session_id.as_deref() else {
            return Err(SessionError::NotResumable(task_run.status));
        };
        if task_run.status == TaskRunStatus::Exited {
            return Err(SessionError::NotResumable(task_run.status));
        }

        let handle = self
            .adapter
            .resume(session_id, text, cfg)
            .map_err(SessionError::Adapter)?;
        task_runs::update_status(&self.pool, task_run_id, TaskRunStatus::Active, None)
            .await
            .map_err(SessionError::Db)?;
        self.spawn_drain(task_run_id.to_string(), handle).await;
        Ok(())
    }

    async fn spawn_drain(self: &Arc<Self>, task_run_id: String, handle: AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let last_activity = Arc::new(Mutex::new(Utc::now()));

        self.sessions.lock().await.insert(
            task_run_id.clone(),
            ActiveSession {
                cmd_tx,
                last_activity: Arc::clone(&last_activity),
            },
        );

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            drain_session(&manager.pool, &task_run_id, handle, cmd_rx, last_activity).await;
            manager.sessions.lock().await.remove(&task_run_id);
        });
    }

    /// Runs the idle reaper forever, closing sessions past `idle_timeout`
    /// every `config.interval` (§4.3). Meant to be spawned as a
    /// background task by the daemon's startup code, alongside
    /// `task_runs::recover_stale_active_runs` at startup.
    pub async fn run_idle_reaper(self: Arc<Self>, config: IdleReaperConfig) {
        self.run_idle_reaper_loop(&config, None).await;
    }

    async fn run_idle_reaper_loop(
        self: &Arc<Self>,
        config: &IdleReaperConfig,
        max_iterations: Option<usize>,
    ) {
        let mut interval = tokio::time::interval(config.interval);
        let mut ran = 0usize;
        loop {
            interval.tick().await;
            self.reap_idle_sessions().await;
            ran += 1;
            if max_iterations.is_some_and(|limit| ran >= limit) {
                return;
            }
        }
    }

    async fn reap_idle_sessions(&self) {
        let now = Utc::now();
        let sessions = self.sessions.lock().await;
        for session in sessions.values() {
            let last_activity = *session.last_activity.lock().await;
            if now - last_activity >= self.idle_timeout {
                let _ = session.cmd_tx.send(Command::Close);
            }
        }
    }
}

/// Owns a live `AgentHandle` exclusively: drains its events into the
/// `events` table (persisting `session_id` as soon as it's known) while
/// also accepting further turns and a close request over `cmd_rx`. Runs
/// until the subprocess exits, then flips the run to `idle` — matching
/// §4.1 step 2 whether that exit was reaper-triggered or the CLI ending
/// its own one-shot turn.
async fn drain_session(
    pool: &SqlitePool,
    task_run_id: &str,
    mut handle: AgentHandle,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    last_activity: Arc<Mutex<DateTime<Utc>>>,
) {
    loop {
        tokio::select! {
            event = handle.recv() => {
                match event {
                    Some(event) => {
                        if let AgentEvent::SessionMeta { session_id } = &event {
                            let _ = task_runs::set_session_id(pool, task_run_id, session_id).await;
                        }
                        let _ = events::append(pool, task_run_id, event.event_type(), event.payload()).await;
                        *last_activity.lock().await = Utc::now();
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Command::Send(text)) => {
                        let _ = handle.send(&text);
                        *last_activity.lock().await = Utc::now();
                    }
                    Some(Command::Close) | None => {
                        handle.close_stdin();
                    }
                }
            }
        }
    }
    let _ = handle.wait().await;
    let _ = task_runs::update_status(pool, task_run_id, TaskRunStatus::Idle, None).await;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration as StdDuration;

    use serde_json::json;

    use super::*;
    use crate::adapter::ClaudeAdapter;
    use crate::db::{connect_in_memory, events, projects, task_runs, tasks};

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    async fn seed_task_run(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: "chat",
                title: "T",
                config: json!({}),
            },
        )
        .await
        .unwrap()
        .id;
        task_runs::create(
            pool,
            task_runs::NewTaskRun {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap()
        .id
    }

    fn role_config() -> RoleConfig {
        RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
        }
    }

    /// Event persistence happens on a spawned background task, so tests
    /// poll with a short bounded retry instead of sleeping a fixed time.
    async fn wait_until_events_len(
        pool: &SqlitePool,
        task_run_id: &str,
        expected: usize,
    ) -> Vec<chokofactory_core::models::Event> {
        for _ in 0..200 {
            let stored = events::list_for_task_run(pool, task_run_id).await.unwrap();
            if stored.len() >= expected {
                return stored;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for {expected} events");
    }

    async fn wait_until_status(pool: &SqlitePool, task_run_id: &str, expected: TaskRunStatus) {
        for _ in 0..200 {
            let run = task_runs::get(pool, task_run_id).await.unwrap().unwrap();
            if run.status == expected {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for status {expected:?}");
    }

    #[tokio::test]
    async fn start_spawns_a_session_and_drains_its_events() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(pool.clone(), adapter, chrono::Duration::hours(1));

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &task_run_id, 2).await;
        assert_eq!(stored[1].payload["text"], "echo:hello");

        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.status, TaskRunStatus::Active);
        assert!(run.session_id.is_some());
    }

    #[tokio::test]
    async fn send_message_forwards_to_an_active_in_memory_session() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(pool.clone(), adapter, chrono::Duration::hours(1));

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        wait_until_events_len(&pool, &task_run_id, 2).await;

        manager
            .send_message(&task_run_id, "again", &role_config())
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &task_run_id, 3).await;
        assert_eq!(stored[2].payload["text"], "echo:again");
    }

    #[tokio::test]
    async fn send_message_resumes_from_a_persisted_session_id_when_not_active_in_memory() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        task_runs::set_session_id(&pool, &task_run_id, "fixed-session-id")
            .await
            .unwrap();
        task_runs::update_status(&pool, &task_run_id, TaskRunStatus::Idle, None)
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(pool.clone(), adapter, chrono::Duration::hours(1));

        manager
            .send_message(&task_run_id, "hello again", &role_config())
            .await
            .unwrap();

        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.status, TaskRunStatus::Active);
        assert_eq!(run.session_id.as_deref(), Some("fixed-session-id"));

        let stored = wait_until_events_len(&pool, &task_run_id, 2).await;
        assert_eq!(stored[1].payload["text"], "echo:hello again");
    }

    #[tokio::test]
    async fn send_message_rejects_an_exited_task_run() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        task_runs::update_status(&pool, &task_run_id, TaskRunStatus::Exited, Some(Utc::now()))
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(pool.clone(), adapter, chrono::Duration::hours(1));

        let err = manager
            .send_message(&task_run_id, "hello", &role_config())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::NotResumable(TaskRunStatus::Exited)
        ));
    }

    #[tokio::test]
    async fn idle_reaper_closes_sessions_past_the_idle_timeout() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: any session is immediately overdue.
        let manager = SessionManager::new(pool.clone(), adapter, chrono::Duration::zero());

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        wait_until_events_len(&pool, &task_run_id, 2).await;

        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;

        wait_until_status(&pool, &task_run_id, TaskRunStatus::Idle).await;
    }
}
