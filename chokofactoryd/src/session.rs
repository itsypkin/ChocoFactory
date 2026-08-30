use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chokofactory_core::models::{TaskRunEndReason, TaskRunStatus};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify, mpsc};

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
    sessions: Mutex<HashMap<String, SessionSlot>>,
    /// Triggered after every successfully-appended event (P1-9), so the
    /// HTTP layer's live-events WebSocket can wake up and re-query instead
    /// of polling. One shared `Notify` for every task rather than a
    /// per-task registry — this is a single-user local daemon with few
    /// concurrent connections, so a global wakeup (each subscriber
    /// re-queries only its own task's rows) is cheap, and avoids a
    /// HashMap-of-notifies whose entries would need their own lifecycle
    /// management (exactly the class of eviction bug this codebase's
    /// reviews keep flagging elsewhere, e.g. `WorkflowEngine::task_locks`).
    events_notify: Arc<Notify>,
}

/// A `sessions` map entry: reserved while a process is being spawned or
/// resumed (so a concurrent caller can't also try to establish one for
/// the same `task_run_id`), then promoted to `Live` once the drain task
/// is actually running.
enum SessionSlot {
    Establishing,
    Live(ActiveSession),
}

struct ActiveSession {
    cmd_tx: mpsc::UnboundedSender<Command>,
    signals: SessionSignals,
}

/// The handles a live session shares between its `sessions` map entry and
/// the detached `drain_session` task that owns its `AgentHandle`. Held as
/// one struct rather than passed around individually so the two sides
/// can't drift out of step about what they share.
#[derive(Clone)]
struct SessionSignals {
    last_activity: Arc<Mutex<DateTime<Utc>>>,
    /// The subprocess's process group id, for `cancel` to signal (#69), or
    /// `None` once it must not be signalled any more.
    ///
    /// Shared and clearable rather than a plain snapshot, because a pid is
    /// only safe to signal until the process is reaped — after that the
    /// number can already belong to something else, and `killpg` would
    /// SIGKILL an unrelated process *group*. `drain_session` clears this
    /// immediately before `handle.wait()` reaps the child, and it holds
    /// this same lock while doing so, so `cancel` either signals a pid
    /// that is still the agent's or finds `None` and signals nothing.
    ///
    /// Without that, the window is real rather than theoretical: the map
    /// slot stays `Live` until `drain_session` has finished reaping *and*
    /// written its status row, and for the first part of that the DB still
    /// says `Active` — so `cancel_task`'s own check would wave a reaped pid
    /// straight through. `shell.rs` guards the identical hazard with
    /// `ProcessGroup::disarm`.
    pgid: Arc<Mutex<Option<u32>>>,
    /// Set by `cancel` immediately *before* it kills the group, and read
    /// by `drain_session` once its loop ends, to record
    /// `TaskRunEndReason::Cancelled` on the run.
    ///
    /// Deliberately not a `Command` on `cmd_tx`: `drain_session`'s
    /// `select!` is `biased` toward draining events, and its own comment
    /// concedes a continuously-emitting turn can delay `cmd_rx`
    /// indefinitely. A cancel starved behind a chatty agent is exactly the
    /// case cancel exists for, so the kill happens inline in `cancel` and
    /// only the *reason* travels through shared state.
    cancelled: Arc<AtomicBool>,
}

enum Command {
    Send(String),
    Close,
}

#[derive(Debug)]
pub enum SessionError {
    UnknownTaskRun,
    NotResumable(TaskRunStatus),
    /// Another call is already spawning or resuming a process for this
    /// `task_run_id`. The caller can retry once that settles.
    AlreadyStarting,
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
            SessionError::AlreadyStarting => {
                write!(
                    f,
                    "a session for this task run is already being established"
                )
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
        events_notify: Arc<Notify>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            adapter,
            idle_timeout,
            sessions: Mutex::new(HashMap::new()),
            events_notify,
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
        self.reserve(task_run_id).await?;

        let handle = match self.adapter.start(prompt, cfg) {
            Ok(handle) => handle,
            Err(err) => {
                self.sessions.lock().await.remove(task_run_id);
                tracing::error!(task_run_id, %err, "failed to start session");
                return Err(SessionError::Adapter(err));
            }
        };
        tracing::info!(task_run_id, "session started");
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
            match sessions.get(task_run_id) {
                Some(SessionSlot::Live(session)) => {
                    *session.signals.last_activity.lock().await = Utc::now();
                    session
                        .cmd_tx
                        .send(Command::Send(text.to_string()))
                        .map_err(|_| SessionError::UnknownTaskRun)?;
                    return Ok(());
                }
                Some(SessionSlot::Establishing) => return Err(SessionError::AlreadyStarting),
                None => {}
            }
        }

        let task_run = task_runs::get(&self.pool, task_run_id)
            .await
            .map_err(SessionError::Db)?
            .ok_or(SessionError::UnknownTaskRun)?;
        let Some(session_id) = task_run.session_id.clone() else {
            return Err(SessionError::NotResumable(task_run.status));
        };
        if task_run.status == TaskRunStatus::Exited {
            return Err(SessionError::NotResumable(task_run.status));
        }

        // Re-checked atomically here (rather than trusting the read
        // above): two concurrent calls for the same not-yet-live
        // task_run_id can both reach this point, but only one of them
        // wins the reservation. The loser reports AlreadyStarting instead
        // of also resuming, which would otherwise spawn a duplicate
        // process and corrupt this map (§ review on PR #28).
        self.reserve(task_run_id).await?;

        let handle = match self.adapter.resume(&session_id, text, cfg) {
            Ok(handle) => handle,
            Err(err) => {
                self.sessions.lock().await.remove(task_run_id);
                tracing::error!(task_run_id, %err, "failed to resume session");
                return Err(SessionError::Adapter(err));
            }
        };
        if let Err(err) =
            task_runs::update_status(&self.pool, task_run_id, TaskRunStatus::Active, None, None)
                .await
        {
            self.sessions.lock().await.remove(task_run_id);
            return Err(SessionError::Db(err));
        }
        tracing::info!(task_run_id, "session resumed");
        self.spawn_drain(task_run_id.to_string(), handle).await;
        Ok(())
    }

    /// Kills `task_run_id`'s live subprocess *and everything it spawned*,
    /// so an operator's cancel actually stops the work (#69).
    ///
    /// Unlike the idle reaper's `Command::Close` — which merely closes
    /// stdin and lets the CLI wind down on its own — this is a SIGKILL to
    /// the whole process group. An agent turn's real weight is in the
    /// commands it starts, and a cancel that left those running would be
    /// cancel in name only.
    ///
    /// Returns `Ok(())` when there is no live process to kill: a run that
    /// already exited, or a task parked on a `human_gate`/`poll`/`terminal`
    /// stage that never opened a session, is already in the state cancel is
    /// trying to reach. `Establishing` is the one case that *is* an error —
    /// another caller is mid-spawn and this call cannot see, and so cannot
    /// kill, the process it is about to create.
    ///
    /// This deliberately does not touch the `task_runs` row.
    /// `drain_session` is that row's single writer, and it records the
    /// `Cancelled` end reason itself once the kill unwinds it; writing the
    /// status here as well would race that write, which
    /// `task_runs::update_status` — an unconditional `UPDATE` with no
    /// expected-status guard — would resolve by silently letting the later
    /// writer win.
    pub async fn cancel(&self, task_run_id: &str) -> Result<(), SessionError> {
        let sessions = self.sessions.lock().await;
        match sessions.get(task_run_id) {
            Some(SessionSlot::Live(session)) => {
                // Ordered before the kill, not after: killing the group
                // closes the subprocess's pipes, which can unwind
                // `drain_session` to its `end_reason` read on another task
                // immediately. Setting the flag afterwards would leave that
                // read racing this write and reporting a cancelled run as
                // an ordinary crash.
                session.signals.cancelled.store(true, Ordering::SeqCst);
                // Held across the kill, and it's the same lock
                // `drain_session` takes to clear the pgid before reaping.
                // That mutual exclusion is what makes the pid safe to
                // signal: either this arrives first and the child is still
                // alive, or the clear arrives first and this sees `None`.
                // Signalling a reaped pid would SIGKILL whatever process
                // group has since been given that number.
                let pgid = session.signals.pgid.lock().await;
                match *pgid {
                    Some(pgid) => {
                        tracing::info!(
                            task_run_id,
                            pgid,
                            "cancelling session: killing process group"
                        );
                        crate::shell::kill_group(pgid);
                    }
                    // Already reaped (or never had a pid): there is no
                    // group left to signal, and no pid safe to signal
                    // *with*.
                    //
                    // The flag set above may well go unread here. The slot
                    // isn't removed from the map until after `drain_session`
                    // writes the run's status, so a cancel landing in that
                    // window finds a `Live` slot whose `cancelled` read has
                    // already happened, and the run records `Reaped`/`None`.
                    // Harmless: the run had already finished on its own, and
                    // `tasks.status` — which every guard keys off — is
                    // written by `cancel_task`, not from here.
                    None => tracing::info!(
                        task_run_id,
                        "cancelling session: process already gone, nothing to kill"
                    ),
                }
                Ok(())
            }
            Some(SessionSlot::Establishing) => Err(SessionError::AlreadyStarting),
            None => Ok(()),
        }
    }

    /// Atomically claims `task_run_id`'s map slot for a caller about to
    /// spawn or resume a process, failing if another caller already holds
    /// it (whether `Establishing` or already `Live`).
    async fn reserve(&self, task_run_id: &str) -> Result<(), SessionError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(task_run_id) {
            return Err(SessionError::AlreadyStarting);
        }
        sessions.insert(task_run_id.to_string(), SessionSlot::Establishing);
        Ok(())
    }

    /// Promotes `task_run_id`'s reserved slot to `Live` and spawns the
    /// task that drains `handle`. Only the caller that won `reserve`
    /// reaches this, so the `insert` here can't race another spawn.
    async fn spawn_drain(self: &Arc<Self>, task_run_id: String, handle: AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let signals = SessionSignals {
            last_activity: Arc::new(Mutex::new(Utc::now())),
            cancelled: Arc::new(AtomicBool::new(false)),
            // Read before `handle` moves into the drain task below — that
            // task owns it exclusively from then on, and `cancel` needs the
            // pgid without being able to reach the handle. Cleared again by
            // `drain_session` the moment the child is about to be reaped.
            pgid: Arc::new(Mutex::new(handle.pid())),
        };

        self.sessions.lock().await.insert(
            task_run_id.clone(),
            SessionSlot::Live(ActiveSession {
                cmd_tx,
                signals: signals.clone(),
            }),
        );

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            drain_session(
                &manager.pool,
                &task_run_id,
                handle,
                cmd_rx,
                signals,
                manager.idle_timeout,
                &manager.events_notify,
            )
            .await;
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
        // Snapshot the live senders/activity handles and release the map
        // lock before awaiting each one, so a reap pass doesn't serialize
        // `start`/`send_message` behind however long that takes.
        let snapshot: Vec<_> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|(task_run_id, slot)| match slot {
                    SessionSlot::Live(session) => Some((
                        task_run_id.clone(),
                        session.cmd_tx.clone(),
                        Arc::clone(&session.signals.last_activity),
                    )),
                    SessionSlot::Establishing => None,
                })
                .collect()
        };

        let now = Utc::now();
        for (task_run_id, cmd_tx, last_activity) in snapshot {
            let last_activity = *last_activity.lock().await;
            if now - last_activity >= self.idle_timeout {
                tracing::info!(
                    task_run_id,
                    "idle reaper: closing session past its idle timeout"
                );
                let _ = cmd_tx.send(Command::Close);
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
    signals: SessionSignals,
    idle_timeout: chrono::Duration,
    events_notify: &Notify,
) {
    let SessionSignals {
        last_activity,
        cancelled,
        pgid,
    } = signals;
    // Once `cmd_rx` closes, `recv()` resolves to `None` immediately on
    // every poll — stop selecting on it (rather than matching `None`
    // inside the loop) so a closed channel can't spin the select! in a
    // tight busy-loop while we wait out the remaining `handle.recv()`s.
    let mut cmd_open = true;
    // Set only when this session's stdin was actually force-closed by the
    // idle reaper (not merely requested — see the staleness re-check
    // below). Distinguishes a reaper-driven clean exit from a turn that
    // genuinely finished on its own, which look identical from `status`
    // alone (§ review on PR #35).
    let mut reaped = false;
    loop {
        tokio::select! {
            // Biased so a pending `handle.recv()` result is always
            // observed before an already-queued `Close` is acted on: with
            // the default randomized selection, a turn that finishes (or
            // emits its final event) right as a stale-triggered `Close` is
            // sitting in `cmd_rx` could have that `Close` processed first,
            // re-check freshness against a `last_activity` that hasn't
            // been bumped yet, wrongly call it stale, and mark a turn that
            // was already finishing on its own as `reaped` (§ review on PR
            // #35). Preferring `handle.recv()` drains any already-ready
            // event/exit first, so `last_activity`/the loop's own `break`
            // reflect the process's real state before `Close` is ever
            // considered.
            //
            // Trade-off accepted: a continuously-emitting turn (events
            // always ready on every poll) could in principle delay
            // `cmd_rx` — a `Send` or the reaper's `Close` — indefinitely,
            // since the event branch always wins ties. This doesn't lose
            // or corrupt anything (no missed `Close`, no wrong
            // `end_reason`), only adds latency, and requires output with
            // no gaps at all between chunks — not how these CLI adapters
            // actually behave in practice — so it's judged acceptable
            // over reintroducing the mislabeling race above.
            biased;
            event = handle.recv() => {
                match event {
                    Some(event) => {
                        if let AgentEvent::SessionMeta { session_id } = &event
                            && let Err(err) = task_runs::set_session_id(pool, task_run_id, session_id).await
                        {
                            tracing::error!(task_run_id, %err, "failed to persist session_id");
                        }
                        match events::append(pool, task_run_id, event.event_type(), event.payload()).await {
                            Ok(event) => {
                                tracing::debug!(task_run_id, event_type = %event.event_type, "appended event");
                                events_notify.notify_waiters();
                            }
                            Err(err) => tracing::error!(task_run_id, %err, "failed to append event"),
                        }
                        // Any drained output counts as activity, not just
                        // inbound `Send`s — broader than §4.1's "no input"
                        // wording, but it's what keeps a mid-turn session
                        // from being reaped out from under itself. A
                        // runaway agent that only ever emits and never
                        // finishes its turn is §5.3's loop-guard's job to
                        // catch, not the idle reaper's.
                        *last_activity.lock().await = Utc::now();
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv(), if cmd_open => {
                match cmd {
                    Some(Command::Send(text)) => {
                        if let Err(err) = handle.send(&text) {
                            tracing::error!(task_run_id, %err, "failed to deliver message, process already gone");
                        }
                        *last_activity.lock().await = Utc::now();
                    }
                    Some(Command::Close) => {
                        // Re-check freshness at the moment this is
                        // actually processed, not when the reaper decided
                        // it: a `Send` (and its last_activity bump) can
                        // land in the queue behind this Close before it's
                        // dequeued, and closing anyway would silently
                        // drop that message once stdin is gone.
                        let stale = Utc::now() - *last_activity.lock().await >= idle_timeout;
                        if stale {
                            handle.close_stdin();
                            reaped = true;
                        }
                    }
                    None => {
                        cmd_open = false;
                    }
                }
            }
        }
    }
    // Retire the pid *before* reaping it, while holding the same lock
    // `cancel` takes to read it. Once `wait` returns, the number can be
    // handed to an unrelated process, and a `cancel` still holding it
    // would SIGKILL that process's whole group. This is the same guard
    // `shell.rs` spells `ProcessGroup::disarm`, and it has to happen here
    // rather than when the map slot is dropped: the slot outlives the reap
    // by the length of the status write below.
    //
    // A `cancel` that wins the race still works — it signals a live
    // process, and the `cancelled` flag it set is read below either way.
    *pgid.lock().await = None;

    // A clean exit (reaper-driven close, or a one-shot agent_turn stage
    // finishing on its own) goes to `idle`, ready to resume. A crash,
    // auth failure, or signal kill goes to `exited` instead — otherwise
    // a deterministic failure would just get resumed into the same
    // crash forever, never reaching a terminal state.
    let exit_status = handle.wait().await;
    let clean_exit = matches!(&exit_status, Ok(status) if status.success());
    if let Err(err) = &exit_status {
        tracing::error!(task_run_id, %err, "failed to reap subprocess");
    }
    let (final_status, ended_at) = if clean_exit {
        (TaskRunStatus::Idle, None)
    } else {
        (TaskRunStatus::Exited, Some(Utc::now()))
    };
    // `status` and `end_reason` are set in the one statement below rather
    // than two: a watcher elsewhere (engine.rs's turn-completion watcher)
    // polls this row from a separate task and must never be able to
    // observe `status == Idle` while `end_reason` still holds a stale (or
    // absent) value from before this exit — that's exactly the gap that
    // would resurrect the ambiguity `end_reason` exists to close.
    //
    // `Cancelled` is checked first and, unlike `Reaped`, without requiring
    // a clean exit — a SIGKILLed process exits by signal, so predicating it
    // on `clean_exit` would record every cancel as an anonymous crash. It
    // also wins over `Reaped` when both could apply (the reaper closed
    // stdin, then an operator cancelled before the process finished
    // winding down): a human asked for this, which is the more useful
    // thing for `choco task status` to say.
    //
    // A turn that finished cleanly in the instant before the signal landed
    // reports `Idle` + `Cancelled`, which reads oddly but is honest, and
    // is harmless: the engine's `tasks.status == "cancelled"` guard in
    // `advance_from_stage`, not this row, is what actually stops a
    // cancelled task from advancing.
    let end_reason = if cancelled.load(Ordering::SeqCst) {
        Some(TaskRunEndReason::Cancelled)
    } else if clean_exit && reaped {
        Some(TaskRunEndReason::Reaped)
    } else {
        None
    };
    if let Err(err) =
        task_runs::update_status(pool, task_run_id, final_status, ended_at, end_reason).await
    {
        tracing::error!(task_run_id, %err, "failed to update status after drain");
    } else {
        tracing::info!(task_run_id, status = %final_status, ?end_reason, "session drained");
    }
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
            sandboxed: false,
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
    async fn a_crashed_subprocess_is_recorded_as_exited_not_idle() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_crash.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();

        // A non-zero exit should land the run in `exited`, not the
        // `idle` (resumable) state a clean reaper-driven close gets.
        wait_until_status(&pool, &task_run_id, TaskRunStatus::Exited).await;
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert!(run.ended_at.is_some());
    }

    #[tokio::test]
    async fn start_spawns_a_session_and_drains_its_events() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

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
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

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
        task_runs::update_status(&pool, &task_run_id, TaskRunStatus::Idle, None, None)
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

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
    async fn a_send_queued_behind_a_stale_reaper_close_is_not_dropped() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // A real timeout, so last_activity looks fresh once the queued
        // Close below is actually dequeued and re-checked.
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        wait_until_events_len(&pool, &task_run_id, 2).await;

        // Simulate the reaper enqueueing a Close based on a stale read of
        // last_activity, taken before the send_message below bumps it -
        // reproduces the ordering from the review finding without
        // depending on real scheduler timing.
        {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&task_run_id) else {
                panic!("session should be live");
            };
            session.cmd_tx.send(Command::Close).unwrap();
        }

        manager
            .send_message(&task_run_id, "again", &role_config())
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &task_run_id, 3).await;
        assert_eq!(stored[2].payload["text"], "echo:again");
    }

    #[tokio::test]
    async fn send_message_rejects_an_exited_task_run() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        task_runs::update_status(
            &pool,
            &task_run_id,
            TaskRunStatus::Exited,
            Some(Utc::now()),
            None,
        )
        .await
        .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

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
    async fn send_message_rejects_a_concurrent_establish_for_the_same_task_run() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        task_runs::set_session_id(&pool, &task_run_id, "fixed-session-id")
            .await
            .unwrap();
        task_runs::update_status(&pool, &task_run_id, TaskRunStatus::Idle, None, None)
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        // Simulate another in-flight call that already claimed the slot
        // between send_message's optimistic map check and its DB read.
        manager.reserve(&task_run_id).await.unwrap();

        let err = manager
            .send_message(&task_run_id, "hello", &role_config())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::AlreadyStarting));
    }

    #[tokio::test]
    async fn send_message_resumes_a_session_the_reaper_previously_idled() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: the reaper closes the session on its first pass.
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

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

        manager
            .send_message(&task_run_id, "again", &role_config())
            .await
            .unwrap();

        // The resumed process is a fresh subprocess too, so it emits its
        // own SessionMeta (event 3) before the AssistantMessage (event 4).
        let stored = wait_until_events_len(&pool, &task_run_id, 4).await;
        assert_eq!(stored[3].payload["text"], "echo:again");
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.status, TaskRunStatus::Active);
    }

    #[tokio::test]
    async fn idle_reaper_closes_sessions_past_the_idle_timeout() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: any session is immediately overdue.
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

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

        // Regression test for the review on PR #35: a reaper-driven clean
        // exit must be distinguishable from a turn that finished on its
        // own, since both land on `Idle` — `end_reason` is what the
        // workflow engine's completion watcher relies on to tell them
        // apart.
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(TaskRunEndReason::Reaped));
    }

    #[tokio::test]
    async fn a_session_that_finishes_on_its_own_has_no_end_reason() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_oneshot.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();

        wait_until_status(&pool, &task_run_id, TaskRunStatus::Idle).await;
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, None);
    }

    #[tokio::test]
    async fn idle_reaper_leaves_sessions_within_the_idle_timeout_active() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

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

        // Give an incorrect teardown a moment to land before asserting
        // the run is still active.
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.status, TaskRunStatus::Active);
    }

    // ---- cancel (#69) ----

    /// Whether `pid` still exists. `kill(pid, 0)` performs the caller's
    /// permission checks and reports whether the target is there, without
    /// actually delivering anything.
    fn process_alive(pid: u32) -> bool {
        // SAFETY: signal 0 delivers nothing; the call only reports whether
        // the process exists, via its return value.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    async fn wait_until_gone(pid: u32) {
        for _ in 0..200 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for pid {pid} to exit");
    }

    /// A wrapper around `fake_claude_spawns_child.py` carrying its two
    /// per-test paths. A generated `sh` script rather than
    /// `std::env::set_var`, for the reason `engine.rs`'s `reply_binary`
    /// already documents: env is process-global and these tests run in
    /// parallel in one process.
    fn spawns_child_binary(dir: &std::path::Path) -> (String, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let heartbeat = dir.join("heartbeat");
        let child_pid = dir.join("child.pid");
        let wrapper = dir.join("fake-claude-spawns-child");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nCHOKO_TEST_HEARTBEAT='{}' CHOKO_TEST_CHILD_PID='{}' exec '{}' \"$@\"\n",
                heartbeat.display(),
                child_pid.display(),
                fixture_binary("fake_claude_spawns_child.py"),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        (wrapper.display().to_string(), heartbeat, child_pid)
    }

    async fn read_pid_when_written(path: &std::path::Path) -> u32 {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for the fixture to report its child pid");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("chokofactoryd-cancel-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn cancel_kills_a_live_session_and_records_it_as_cancelled() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        wait_until_events_len(&pool, &task_run_id, 2).await;

        manager.cancel(&task_run_id).await.unwrap();

        wait_until_status(&pool, &task_run_id, TaskRunStatus::Exited).await;
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        // The distinction that matters: a SIGKILLed process exits
        // non-zero, which is indistinguishable from a crash by `status`
        // alone. `end_reason` is what tells an operator their cancel is
        // what stopped it.
        assert_eq!(run.end_reason, Some(TaskRunEndReason::Cancelled));
        assert!(run.ended_at.is_some());
    }

    /// The reason the adapter spawns into its own process group: an agent
    /// turn's real weight is in the commands it starts, and reaping only
    /// the process the daemon spawned would leave those running in the
    /// task's working copy after the operator was told it stopped.
    #[tokio::test]
    async fn cancel_kills_the_whole_process_group_not_just_the_agent() {
        let dir = TempDir::new();
        let (binary, heartbeat, child_pid_path) = spawns_child_binary(&dir.0);

        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "go", &role_config())
            .await
            .unwrap();

        let child_pid = read_pid_when_written(&child_pid_path).await;
        assert!(
            process_alive(child_pid),
            "the fixture's child should be running before cancel"
        );

        manager.cancel(&task_run_id).await.unwrap();

        // The grandchild, not just the agent: this is the assertion that
        // would fail if `cancel` used `child.kill()` instead of `killpg`.
        wait_until_gone(child_pid).await;

        // And it really stopped working, rather than merely leaving the
        // process table: no further heartbeats after a settling pause.
        let after_kill = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        let later = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            after_kill, later,
            "the killed child should have stopped writing its heartbeat"
        );

        wait_until_status(&pool, &task_run_id, TaskRunStatus::Exited).await;
        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(TaskRunEndReason::Cancelled));
    }

    /// A turn that ignores stdin entirely is exactly what the idle
    /// reaper's `close_stdin` cannot stop, so cancel must not depend on
    /// the subprocess cooperating.
    #[tokio::test]
    async fn cancel_stops_a_turn_that_never_reads_its_stdin() {
        let dir = TempDir::new();
        let (binary, _heartbeat, child_pid_path) = spawns_child_binary(&dir.0);

        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "go", &role_config())
            .await
            .unwrap();
        read_pid_when_written(&child_pid_path).await;

        manager.cancel(&task_run_id).await.unwrap();

        wait_until_status(&pool, &task_run_id, TaskRunStatus::Exited).await;
    }

    /// Cancelling a run with no live process is the state cancel is trying
    /// to reach, so it succeeds rather than erroring — otherwise the engine
    /// would have to special-case every `human_gate`/`poll`/`terminal`
    /// stage and every already-exited run before daring to call this.
    #[tokio::test]
    async fn cancel_is_a_no_op_for_a_run_with_no_live_session() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager.cancel(&task_run_id).await.unwrap();
        manager.cancel("no-such-run").await.unwrap();
    }

    /// A session mid-spawn is the one case that must *not* report success:
    /// the process doesn't exist yet and isn't reachable from the map, so
    /// answering `Ok` would tell an operator an agent was stopped while it
    /// was in fact just starting.
    #[tokio::test]
    async fn cancel_rejects_a_run_whose_session_is_still_being_established() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager.reserve(&task_run_id).await.unwrap();

        let err = manager.cancel(&task_run_id).await.unwrap_err();
        assert!(matches!(err, SessionError::AlreadyStarting));
    }

    /// A cancel arriving while the idle reaper had already closed stdin:
    /// both reasons could claim the run, and `Cancelled` must win, because
    /// `Reaped` would tell an operator their cancel did nothing.
    ///
    /// Going through `cancel` would *not* pin this. Its SIGKILL makes the
    /// exit non-clean, and `Reaped` requires `clean_exit`, so it loses on
    /// that alone and the test would still pass with the two arms
    /// swapped. The case where precedence actually decides is a process
    /// that exits *cleanly* — reaper-closed stdin — with the cancel flag
    /// also set, which is what setting the flag directly (rather than
    /// killing) constructs here.
    #[tokio::test]
    async fn cancelled_beats_reaped_when_a_cancelled_session_still_exits_cleanly() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: the reaper closes this session on its first pass.
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        wait_until_events_len(&pool, &task_run_id, 2).await;

        // The flag without the kill: stands in for a cancel whose SIGKILL
        // lands just after the process has already wound down on its own.
        {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&task_run_id) else {
                panic!("session should be live");
            };
            session.signals.cancelled.store(true, Ordering::SeqCst);
        }

        // Now let the reaper close stdin, so the process exits cleanly and
        // `reaped` is set too — both reasons in play at once.
        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;
        wait_until_status(&pool, &task_run_id, TaskRunStatus::Idle).await;

        let run = task_runs::get(&pool, &task_run_id).await.unwrap().unwrap();
        assert_eq!(
            run.end_reason,
            Some(TaskRunEndReason::Cancelled),
            "a clean exit with both flags set must report the operator's cancel, not the reaper"
        );
    }

    /// The pid must be retired before the child is reaped, or a `cancel`
    /// arriving in the window between `wait()` and the map slot being
    /// dropped would `killpg` a number the OS may have already reused —
    /// SIGKILLing an unrelated process group.
    #[tokio::test]
    async fn a_reaped_sessions_pgid_is_cleared_so_cancel_cannot_signal_it() {
        let pool = connect_in_memory().await.unwrap();
        let task_run_id = seed_task_run(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_oneshot.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        // Grab the shared pgid handle while the session is live, so it can
        // still be inspected after the map slot is gone.
        manager
            .start(&task_run_id, "hello", &role_config())
            .await
            .unwrap();
        let pgid = {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&task_run_id) else {
                panic!("session should be live");
            };
            assert!(
                session.signals.pgid.lock().await.is_some(),
                "a live session should have a pgid to signal"
            );
            Arc::clone(&session.signals.pgid)
        };

        // `fake_claude_oneshot.py` exits on its own, so the drain loop
        // reaps it without any cancel involved.
        wait_until_status(&pool, &task_run_id, TaskRunStatus::Idle).await;

        assert!(
            pgid.lock().await.is_none(),
            "the pgid must be cleared before the child is reaped, or a later \
             cancel could signal a reused pid"
        );
    }
}
