use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::proc_table::{self, Identity, OwnershipInput, ProcEntry, ProcReader};
use crate::shell::GroupKill;

use chocofactory_core::models::{EventType, SessionEndReason, SessionStatus};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::adapter::{
    AdapterError, AgentEvent, AgentHandle, BackgroundJob, Registry, RoleConfig, UnknownCliError,
    describe_jobs,
};
use crate::db::{events, sessions, usage};

/// Drives the active ⇄ idle ⇄ resume state machine (§4.1) on top of
/// `sessions`: keeps a live `AgentHandle` per active `session_id`,
/// drains its events into the `events` table, and resumes a fresh
/// process from the persisted `adapter_session_id` when a message arrives for a
/// run that isn't currently live in memory.
pub struct SessionManager {
    pool: SqlitePool,
    registry: Registry,
    idle_timeout: chrono::Duration,
    turn_timers: TurnTimers,
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
    /// Set by [`Self::shutdown`] before it snapshots the live sessions;
    /// once set, nothing new may start. Cloned into every session's
    /// `SessionSignals::stopping`.
    shutting_down: Arc<AtomicBool>,
    /// Reads the process table for the leftover sweeps. Swapped in tests.
    proc_reader: ProcReader,
}

/// A `sessions` map entry: reserved while a process is being spawned or
/// resumed (so a concurrent caller can't also try to establish one for
/// the same `session_id`), then promoted to `Live` once the drain task
/// is actually running.
enum SessionSlot {
    Establishing,
    Live(ActiveSession),
}

/// Whether a session's owning `agent_turn` stage can conclude on its own
/// (a plain single-shot turn, or one with `capture:` — §5.2) or stays open
/// indefinitely for further live messages (`on: {}`, chat, §5.4). Threaded
/// down to `drain_session` because only the former has a completion to
/// detect (a `result` after a `report_outcome` call, #90) and a stdin to
/// close once it happens: doing that to a standing-open chat session would
/// cut it off after its very first reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    SingleShot,
    Standing,
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
    /// `SessionEndReason::Cancelled` on the run.
    ///
    /// Deliberately not a `Command` on `cmd_tx`: `drain_session`'s
    /// `select!` is `biased` toward draining events, and its own comment
    /// concedes a continuously-emitting turn can delay `cmd_rx`
    /// indefinitely. A cancel starved behind a chatty agent is exactly the
    /// case cancel exists for, so the kill happens inline in `cancel` and
    /// only the *reason* travels through shared state.
    cancelled: Arc<AtomicBool>,
    /// Set (shared with `SessionManager::shutting_down`) when the daemon is
    /// stopping. Read by `drain_session` to record `DaemonStopped`; unlike
    /// `cancelled` it is never set per run, so it can't make an operator's
    /// cancel look like a shutdown (`Cancelled` outranks it).
    stopping: Arc<AtomicBool>,
    /// What this turn's kills and sweeps found, for the notes
    /// `drain_session` writes at its end.
    tracker: Arc<LeftoverTracker>,
}

enum Command {
    Send(String),
    Close,
}

#[derive(Debug)]
pub enum SessionError {
    UnknownSession,
    NotResumable(SessionStatus),
    /// Another call is already spawning or resuming a process for this
    /// `session_id`. The caller can retry once that settles.
    AlreadyStarting,
    /// The daemon is shutting down and starts nothing new.
    ShuttingDown,
    /// The CLI named for the session isn't in the registry. Nothing was
    /// reserved, spawned or written.
    UnknownCli(UnknownCliError),
    /// The session's adapter can't run the role as configured now. Nothing
    /// was reserved, spawned or written.
    RoleRejected(String),
    Adapter(AdapterError),
    Db(sqlx::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::UnknownSession => write!(f, "no such session"),
            SessionError::NotResumable(status) => {
                write!(
                    f,
                    "session is {status} and has no adapter session to resume"
                )
            }
            SessionError::AlreadyStarting => {
                write!(f, "this session is already being established")
            }
            SessionError::ShuttingDown => write!(f, "the daemon is shutting down"),
            SessionError::UnknownCli(err) => write!(f, "{err}"),
            SessionError::RoleRejected(message) => write!(f, "{message}"),
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
        registry: Registry,
        idle_timeout: chrono::Duration,
        events_notify: Arc<Notify>,
    ) -> Arc<Self> {
        Self::with_turn_timers(
            pool,
            registry,
            idle_timeout,
            events_notify,
            TurnTimers::default(),
        )
    }

    /// [`Self::new`] with non-default [`TurnTimers`], so tests can exercise
    /// nudges and the grace kill in milliseconds rather than minutes.
    pub fn with_turn_timers(
        pool: SqlitePool,
        registry: Registry,
        idle_timeout: chrono::Duration,
        events_notify: Arc<Notify>,
        turn_timers: TurnTimers,
    ) -> Arc<Self> {
        Self::build(
            pool,
            registry,
            idle_timeout,
            events_notify,
            turn_timers,
            Arc::new(proc_table::read),
        )
    }

    /// [`Self::with_turn_timers`] with a substitute process-table reader.
    #[cfg(test)]
    pub(crate) fn with_proc_reader(
        pool: SqlitePool,
        registry: Registry,
        idle_timeout: chrono::Duration,
        events_notify: Arc<Notify>,
        turn_timers: TurnTimers,
        proc_reader: ProcReader,
    ) -> Arc<Self> {
        Self::build(
            pool,
            registry,
            idle_timeout,
            events_notify,
            turn_timers,
            proc_reader,
        )
    }

    fn build(
        pool: SqlitePool,
        registry: Registry,
        idle_timeout: chrono::Duration,
        events_notify: Arc<Notify>,
        turn_timers: TurnTimers,
        proc_reader: ProcReader,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            idle_timeout,
            turn_timers,
            sessions: Mutex::new(HashMap::new()),
            events_notify,
            shutting_down: Arc::new(AtomicBool::new(false)),
            proc_reader,
        })
    }

    /// The adapters this manager can dispatch to.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Starts a brand-new subprocess for `session_id` and begins
    /// draining its events (§4.1 step 1). The caller is responsible for
    /// having already created the `sessions` row (it's created `active`
    /// by `sessions::create`).
    ///
    /// `kind` decides how `drain_session` reacts to the CLI's own
    /// end-of-turn marker (#70) — see [`SessionKind`].
    pub async fn start(
        self: &Arc<Self>,
        session_id: &str,
        cli: &str,
        prompt: &str,
        cfg: &RoleConfig,
        kind: SessionKind,
    ) -> Result<(), SessionError> {
        let adapter = self
            .registry
            .lookup(None, cli)
            .map_err(SessionError::UnknownCli)?
            .clone();
        self.reserve(session_id).await?;

        let handle = match adapter.start(prompt, cfg) {
            Ok(handle) => handle,
            Err(err) => {
                self.sessions.lock().await.remove(session_id);
                tracing::error!(session_id, %err, "failed to start session");
                return Err(SessionError::Adapter(err));
            }
        };
        tracing::info!(session_id, "session started");
        self.spawn_drain(session_id.to_string(), handle, kind).await;
        Ok(())
    }

    /// Starts a subprocess for `session_id` that *continues* `adapter_session_id`
    /// rather than opening a fresh session (#92), and begins draining it.
    /// The caller has already created the `sessions` row, as for
    /// [`Self::start`]; `adapter_session_id` comes from the earlier run whose turn
    /// was interrupted, and the new row keeps its own status.
    ///
    /// Separate from [`Self::send_message`]'s resume path on purpose. That
    /// one resumes *the same run* — it refuses an `Exited` row, flips the
    /// row back to `Active`, and always drains as
    /// [`SessionKind::Standing`], because the only thing that ever resumed
    /// before this was a chat session. A retry resumes an interrupted
    /// single-shot turn into a *new* run, so none of those three apply.
    pub async fn resume(
        self: &Arc<Self>,
        session_id: &str,
        cli: &str,
        adapter_session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
        kind: SessionKind,
    ) -> Result<(), SessionError> {
        let adapter = self
            .registry
            .lookup(None, cli)
            .map_err(SessionError::UnknownCli)?
            .clone();
        self.reserve(session_id).await?;

        let handle = match adapter.resume(adapter_session_id, prompt, cfg) {
            Ok(handle) => handle,
            Err(err) => {
                self.sessions.lock().await.remove(session_id);
                tracing::error!(session_id, adapter_session_id, %err, "failed to resume session");
                return Err(SessionError::Adapter(err));
            }
        };
        tracing::info!(
            session_id,
            adapter_session_id,
            "session resumed into a new run"
        );
        self.spawn_drain(session_id.to_string(), handle, kind).await;
        Ok(())
    }

    /// Sends a message to `session_id`. If the run has a live subprocess
    /// in memory, forwards straight to its stdin. Otherwise resumes a
    /// fresh process from the persisted `adapter_session_id` (§4.1 step 3) and
    /// flips the run back to `active`.
    pub async fn send_message(
        self: &Arc<Self>,
        session_id: &str,
        text: &str,
        cfg: &RoleConfig,
    ) -> Result<(), SessionError> {
        {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(SessionSlot::Live(session)) => {
                    *session.signals.last_activity.lock().await = Utc::now();
                    session
                        .cmd_tx
                        .send(Command::Send(text.to_string()))
                        .map_err(|_| SessionError::UnknownSession)?;
                    return Ok(());
                }
                Some(SessionSlot::Establishing) => return Err(SessionError::AlreadyStarting),
                None => {}
            }
        }

        let session_row = sessions::get(&self.pool, session_id)
            .await
            .map_err(SessionError::Db)?
            .ok_or(SessionError::UnknownSession)?;
        let Some(adapter_session_id) = session_row.adapter_session_id.clone() else {
            return Err(SessionError::NotResumable(session_row.status));
        };
        if session_row.status == SessionStatus::Exited {
            return Err(SessionError::NotResumable(session_row.status));
        }

        // The session's own recorded CLI, never the role's current one: a
        // session id belongs to the CLI that created it.
        let adapter = self
            .registry
            .lookup(Some(session_row.role.as_str()), &session_row.cli_adapter)
            .map_err(SessionError::UnknownCli)?
            .clone();
        adapter
            .validate_role(&session_row.role, &cfg.isolation)
            .map_err(SessionError::RoleRejected)?;

        // Re-checked atomically here (rather than trusting the read
        // above): two concurrent calls for the same not-yet-live
        // session_id can both reach this point, but only one of them
        // wins the reservation. The loser reports AlreadyStarting instead
        // of also resuming, which would otherwise spawn a duplicate
        // process and corrupt this map (§ review on PR #28).
        self.reserve(session_id).await?;

        let handle = match adapter.resume(&adapter_session_id, text, cfg) {
            Ok(handle) => handle,
            Err(err) => {
                self.sessions.lock().await.remove(session_id);
                tracing::error!(session_id, %err, "failed to resume session");
                return Err(SessionError::Adapter(err));
            }
        };
        if let Err(err) =
            sessions::update_status(&self.pool, session_id, SessionStatus::Active, None, None).await
        {
            self.sessions.lock().await.remove(session_id);
            return Err(SessionError::Db(err));
        }
        tracing::info!(session_id, "session resumed");
        // Only reachable for an open-ended stage: `send_message_or_resume`
        // routes a single-shot `agent_turn` elsewhere before this can ever
        // be called (`engine.rs`'s `stage_def.on.is_empty()` check), so a
        // resumed session is always standing-open (chat-shaped).
        self.spawn_drain(session_id.to_string(), handle, SessionKind::Standing)
            .await;
        Ok(())
    }

    /// Kills `session_id`'s live subprocess *and everything it spawned*,
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
    /// This deliberately does not touch the `sessions` row.
    /// `drain_session` is that row's single writer, and it records the
    /// `Cancelled` end reason itself once the kill unwinds it; writing the
    /// status here as well would race that write, which
    /// `sessions::update_status` — an unconditional `UPDATE` with no
    /// expected-status guard — would resolve by silently letting the later
    /// writer win.
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
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
                            session_id,
                            pgid,
                            "cancelling session: killing process group"
                        );
                        kill_and_sweep(&session.signals.tracker, pgid, "cancel").await;
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
                        session_id,
                        "cancelling session: process already gone, nothing to kill"
                    ),
                }
                Ok(())
            }
            Some(SessionSlot::Establishing) => Err(SessionError::AlreadyStarting),
            None => Ok(()),
        }
    }

    /// Atomically claims `session_id`'s map slot for a caller about to
    /// spawn or resume a process, failing if another caller already holds
    /// it (whether `Establishing` or already `Live`).
    async fn reserve(&self, session_id: &str) -> Result<(), SessionError> {
        // Before the slot exists: a start that loses to `shutdown` here
        // never spawns anything.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(SessionError::ShuttingDown);
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(session_id) {
            return Err(SessionError::AlreadyStarting);
        }
        sessions.insert(session_id.to_string(), SessionSlot::Establishing);
        Ok(())
    }

    /// Promotes `session_id`'s reserved slot to `Live` and spawns the
    /// task that drains `handle`. Only the caller that won `reserve`
    /// reaches this, so the `insert` here can't race another spawn.
    async fn spawn_drain(
        self: &Arc<Self>,
        session_id: String,
        handle: AgentHandle,
        kind: SessionKind,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let signals = SessionSignals {
            last_activity: Arc::new(Mutex::new(Utc::now())),
            cancelled: Arc::new(AtomicBool::new(false)),
            stopping: Arc::clone(&self.shutting_down),
            // Read before `handle` moves into the drain task below — that
            // task owns it exclusively from then on, and `cancel` needs the
            // pgid without being able to reach the handle. Cleared again by
            // `drain_session` the moment the child is about to be reaped.
            pgid: Arc::new(Mutex::new(handle.pid())),
            tracker: Arc::new(LeftoverTracker::new(
                handle.marker().to_string(),
                kind,
                Arc::clone(&self.proc_reader),
            )),
        };

        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionSlot::Live(ActiveSession {
                cmd_tx,
                signals: signals.clone(),
            }),
        );

        // Re-check after the insert. `shutdown` sets the flag before it
        // snapshots the map, and the insert above happens before this
        // read, so either `shutdown`'s snapshot saw this slot (and kills
        // it) or this read sees the flag (and kills it here): a session
        // that slipped in between `reserve` and now is never left running.
        if self.shutting_down.load(Ordering::SeqCst) {
            let pgid = signals.pgid.lock().await;
            if let Some(pgid) = *pgid {
                tracing::info!(
                    session_id,
                    pgid,
                    "daemon is shutting down: killing a session that had just started"
                );
                kill_and_sweep(&signals.tracker, pgid, "shutdown race").await;
            }
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            drain_session(
                &manager.pool,
                &session_id,
                handle,
                kind,
                cmd_rx,
                signals,
                manager.idle_timeout,
                &manager.turn_timers,
                &manager.events_notify,
            )
            .await;
            manager.sessions.lock().await.remove(&session_id);
        });
    }

    /// Graceful shutdown: refuses new sessions, SIGKILLs the process group
    /// of every live one, and waits up to `grace` for their drain tasks to
    /// record the outcome (`DaemonStopped`) and leave the map.
    ///
    /// Deliberately does not set any run's `cancelled` flag: this is the
    /// daemon stopping, not an operator's cancel, and the two end reasons
    /// resume differently in the UI.
    pub async fn shutdown(&self, grace: Duration) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Snapshot the pgid handles and release the map lock before
        // awaiting each, as `reap_idle_sessions` does.
        let snapshot: Vec<_> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|(session_id, slot)| match slot {
                    SessionSlot::Live(session) => Some((
                        session_id.clone(),
                        Arc::clone(&session.signals.pgid),
                        Arc::clone(&session.signals.tracker),
                    )),
                    SessionSlot::Establishing => None,
                })
                .collect()
        };
        let mut killed = 0usize;
        for (session_id, pgid, tracker) in snapshot {
            // Same lock `drain_session` takes to retire the pid before
            // reaping, so a reaped pid is never signalled.
            let pgid = pgid.lock().await;
            if let Some(pgid) = *pgid {
                tracing::info!(session_id, pgid, "shutdown: killing session process group");
                kill_and_sweep(&tracker, pgid, "shutdown").await;
                killed += 1;
            }
        }
        let deadline = tokio::time::Instant::now() + grace;
        let remaining = loop {
            let remaining = self.sessions.lock().await.len();
            if remaining == 0 || tokio::time::Instant::now() >= deadline {
                break remaining;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        if remaining > 0 {
            tracing::warn!(
                killed,
                remaining,
                "session manager shut down with sessions still draining"
            );
        } else {
            tracing::info!(killed, remaining, "session manager shut down");
        }
    }

    /// Runs the idle reaper forever, closing sessions past `idle_timeout`
    /// every `config.interval` (§4.3). Meant to be spawned as a
    /// background task by the daemon's startup code, alongside
    /// `sessions::recover_stale_active_sessions` at startup.
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
                .filter_map(|(session_id, slot)| match slot {
                    SessionSlot::Live(session) => Some((
                        session_id.clone(),
                        session.cmd_tx.clone(),
                        Arc::clone(&session.signals.last_activity),
                    )),
                    SessionSlot::Establishing => None,
                })
                .collect()
        };

        let now = Utc::now();
        for (session_id, cmd_tx, last_activity) in snapshot {
            let last_activity = *last_activity.lock().await;
            if now - last_activity >= self.idle_timeout {
                tracing::info!(
                    session_id,
                    "idle reaper: closing session past its idle timeout"
                );
                let _ = cmd_tx.send(Command::Close);
            }
        }
    }
}

/// Owns a live `AgentHandle` exclusively: drains its events into the
/// `events` table (persisting `adapter_session_id` as soon as it's known) while
/// also accepting further turns and a close request over `cmd_rx`. Runs
/// until the subprocess exits, then records the run's final status — `idle`
/// for a clean finish, matching §4.1 step 2 whether that exit was
/// reaper-triggered or the turn's own.
///
/// ## When a [`SessionKind::SingleShot`] turn is done (#90)
///
/// The CLI's `result` line does not mean the work is done. Claude Code's own
/// way to wait on background work is to start it, end the turn, and be woken
/// by a notification when it finishes — which is exactly how, in #88, a
/// coder handed its job to a background sub-agent, printed `result` with
/// nothing committed, and had the workflow advance past it while the
/// sub-agent kept writing to the worktree. So a single-shot turn completes
/// only on a `result` that follows a successful `report_outcome` call by the
/// main agent (not a sub-agent, not one the tool rejected):
///
/// 1. Report, then `result`: the turn is complete. Stdin is closed so the CLI
///    exits, and a grace timer starts. If the process is still alive when it
///    runs out, its whole group is killed and the run ends `Lingered`: the
///    turn said it was done while something it started kept going.
/// 2. `result` with no report: the turn is waiting (or forgot). Stdin stays
///    open, so a background job's notification can wake it. After
///    `nudge_after` with no events at all it is nudged on stdin, up to
///    `max_nudges` times; after that, stdin is closed, the grace rule
///    applies, and the run ends `NoReport`. While the CLI reports background
///    jobs still running (claude only), the turn is not nudged: it is closed
///    the same way if still waiting `job_wait_limit` after it started
///    waiting, and the idle reaper leaves it alone (#271).
/// 3. A `result` with `is_error` closes stdin and applies the grace rule; the
///    run ends `exited`, as a crash would.
///
/// `idle` is written only once the process has actually gone, so the engine's
/// watcher can never advance a workflow while the turn's process is still
/// running. Anything the process emits after completion is still recorded,
/// flagged `after_completion`, rather than silently appended to a run the
/// engine may already be acting on.
///
/// A [`SessionKind::Standing`] session (chat) ignores `result` entirely — it
/// sees one per turn, and closing stdin after the first would end the
/// conversation.
#[allow(clippy::too_many_arguments)]
async fn drain_session(
    pool: &SqlitePool,
    session_id: &str,
    mut handle: AgentHandle,
    kind: SessionKind,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    signals: SessionSignals,
    idle_timeout: chrono::Duration,
    turn_timers: &TurnTimers,
    events_notify: &Notify,
) {
    let SessionSignals {
        last_activity,
        cancelled,
        stopping,
        pgid,
        tracker,
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
    let mut turn = SingleShotTurn::default();
    // Deadlines that are not armed sit here, far enough out never to fire.
    // `select!` evaluates a disabled branch's future expression anyway, so
    // each timer needs *some* instant even when its guard is false.
    let never = tokio::time::Instant::now() + Duration::from_secs(60 * 60 * 24 * 365);
    loop {
        let nudge_at = turn.last_event_at + turn_timers.nudge_after;
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
            // considered. The same ordering keeps an event that arrives
            // right at a nudge or grace deadline from being missed by it.
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
                let Some(event) = event else { break };
                turn.last_event_at = tokio::time::Instant::now();
                if let AgentEvent::SessionMeta { adapter_session_id, .. } = &event
                    && let Err(err) = sessions::set_adapter_session_id(pool, session_id, adapter_session_id).await
                {
                    tracing::error!(session_id, %err, "failed to persist adapter_session_id");
                }
                // Recorded while the agent is alive and its children are still
                // its children: a background job's shell is the CLI's child
                // only until the CLI exits.
                if kind == SessionKind::SingleShot
                    && matches!(
                        event,
                        AgentEvent::BackgroundJobs { .. } | AgentEvent::TurnCompleted { .. }
                    )
                    && let Some(agent) = *pgid.lock().await
                {
                    tracker.record_descendants(agent, "turn event").await;
                }
                let event_type = event.event_type();
                let mut payload = event.payload();
                if turn.completed
                    && let Value::Object(map) = &mut payload
                {
                    map.insert("after_completion".to_string(), Value::Bool(true));
                }
                // A top-level `turn_completed` and its usage row commit
                // together (or not at all); a sub-agent's records no usage,
                // since the main conversation's cost and per-model figures
                // already include it (its tokens come from those figures).
                let appended = match &event {
                    AgentEvent::TurnCompleted { usage, .. } => {
                        usage::append_turn_completed(pool, session_id, payload, usage).await
                    }
                    _ => events::append(pool, session_id, event_type, payload).await,
                };
                match appended {
                    Ok(appended) => {
                        tracing::debug!(session_id, event_type = %appended.event_type, "appended event");
                        events_notify.notify_waiters();
                    }
                    Err(err) => tracing::error!(session_id, %err, "failed to append event"),
                }
                if kind == SessionKind::SingleShot {
                    match turn.observe(&event) {
                        TurnStep::Continue => {}
                        TurnStep::Completed => {
                            tracing::info!(
                                session_id,
                                "single-shot turn reported and ended; closing stdin"
                            );
                            handle.close_stdin();
                            turn.arm_grace(turn_timers.grace, GraceCause::Completed);
                        }
                        TurnStep::EndedWithError => {
                            handle.close_stdin();
                            turn.arm_grace(turn_timers.grace, GraceCause::Errored);
                        }
                        TurnStep::WaitingForReport => tracing::info!(
                            session_id,
                            "single-shot turn ended without reporting; leaving it open"
                        ),
                    }
                    let now = tokio::time::Instant::now();
                    match turn.update_job_wait(now, turn_timers.job_wait_limit) {
                        JobWaitChange::Entered { remaining } => {
                            note_job_wait_entered(
                                pool,
                                session_id,
                                &turn.background_jobs,
                                turn_timers.job_wait_limit,
                                remaining,
                                events_notify,
                            )
                            .await;
                        }
                        JobWaitChange::ExhaustedOnEntry => {
                            close_after_job_wait(
                                pool,
                                session_id,
                                &mut handle,
                                &mut turn,
                                turn_timers,
                                true,
                                events_notify,
                            )
                            .await;
                        }
                        JobWaitChange::NoChange => {}
                    }
                    // Also checked here, not only on the timer branch: the
                    // biased select could starve that branch under output
                    // with no gaps.
                    if turn.job_wait_expired(now) {
                        close_after_job_wait(
                            pool,
                            session_id,
                            &mut handle,
                            &mut turn,
                            turn_timers,
                            false,
                            events_notify,
                        )
                        .await;
                    }
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
            cmd = cmd_rx.recv(), if cmd_open => {
                match cmd {
                    Some(Command::Send(text)) => {
                        if let Err(err) = handle.send(&text) {
                            tracing::error!(session_id, %err, "failed to deliver message, process already gone");
                        }
                        *last_activity.lock().await = Utc::now();
                    }
                    // The idle reaper's `Close` is ignored while the turn waits
                    // on its background jobs: nothing nudges it any more, so
                    // nothing else keeps `last_activity` fresh. The job-wait
                    // deadline bounds it instead (#271).
                    Some(Command::Close)
                        if kind == SessionKind::SingleShot && turn.waiting_on_jobs() =>
                    {
                        tracing::debug!(session_id, "ignoring the idle reaper: the turn is waiting on background jobs");
                    }
                    // A `Close` that lands after the turn already completed
                    // is a no-op: stdin is already closed, and `reaped` must
                    // never describe a run that in fact finished cleanly.
                    Some(Command::Close) if !turn.completed => {
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
                            if kind == SessionKind::SingleShot {
                                turn.arm_grace(turn_timers.grace, GraceCause::Reaped);
                            }
                        }
                    }
                    Some(Command::Close) => {}
                    None => {
                        cmd_open = false;
                    }
                }
            }
            _ = tokio::time::sleep_until(turn.job_wait_deadline.unwrap_or(never)),
                if turn.job_wait_deadline.is_some() =>
            {
                if turn.job_wait_expired(tokio::time::Instant::now()) {
                    close_after_job_wait(
                        pool,
                        session_id,
                        &mut handle,
                        &mut turn,
                        turn_timers,
                        false,
                        events_notify,
                    )
                    .await;
                }
            }
            _ = tokio::time::sleep_until(nudge_at), if turn.should_nudge() => {
                if turn.nudges_sent < turn_timers.max_nudges {
                    turn.nudges_sent += 1;
                    turn.last_event_at = tokio::time::Instant::now();
                    append_session_note(
                        pool,
                        session_id,
                        "nudge",
                        &format!(
                            "the turn ended without calling report_outcome; asked it to report \
                             (nudge {} of {})",
                            turn.nudges_sent, turn_timers.max_nudges
                        ),
                        events_notify,
                    )
                    .await;
                    if let Err(err) = handle.send(NUDGE_TEXT) {
                        tracing::error!(session_id, %err, "failed to nudge the turn, process already gone");
                    }
                    *last_activity.lock().await = Utc::now();
                } else {
                    turn.gave_up = true;
                    tracing::warn!(session_id, "single-shot turn never reported; closing it");
                    append_session_note(
                        pool,
                        session_id,
                        "no_report",
                        &format!(
                            "the turn never called report_outcome, even after {} nudge(s); \
                             closing it",
                            turn.nudges_sent
                        ),
                        events_notify,
                    )
                    .await;
                    handle.close_stdin();
                    turn.arm_grace(turn_timers.grace, GraceCause::NoReport);
                }
            }
            _ = tokio::time::sleep_until(turn.grace_deadline.unwrap_or(never)),
                if turn.grace_deadline.is_some() && turn.kill_settle_deadline.is_none() =>
            {
                // Held across the kill, the same lock `cancel` takes and the
                // tail below clears before reaping. The pid can't have been
                // reaped yet — nothing reaps before this loop ends — so
                // `Some` here is still this session's group, even if the
                // process itself has just exited: an unreaped zombie keeps
                // its pid, so the group number can't have been reused.
                //
                // A process that exits in the same instant this fires, with
                // its stream's end not yet observed above, is still recorded
                // as lingering. That errs towards parking the task, which a
                // human can retry, rather than towards advancing.
                let report = match *pgid.lock().await {
                    Some(pgid) => Some(kill_and_sweep(&tracker, pgid, "grace").await),
                    None => None,
                };
                if let Some(report) = &report {
                    turn.lingered = lingered_after(report);
                }
                if turn.lingered {
                    let after = match turn.grace_cause {
                        Some(GraceCause::Reaped) => "the idle reaper closed its stalled turn",
                        Some(GraceCause::NoReport) => "it was closed for never reporting",
                        Some(GraceCause::Completed) | Some(GraceCause::Errored) | None => {
                            "its turn ended"
                        }
                    };
                    tracing::warn!(
                        session_id,
                        grace_ms = turn_timers.grace.as_millis() as u64,
                        cause = after,
                        "process still running after stdin was closed; tried to kill its process group"
                    );
                    let grace_s = turn_timers.grace.as_secs_f32();
                    let message = match report.as_ref().map(|r| &r.group) {
                        Some(GroupKill::Killed(_)) => format!(
                            "the agent process was still running {grace_s:.1}s after {after}; \
                             killed its process group"
                        ),
                        Some(GroupKill::Failed(err)) => format!(
                            "the agent process was still running {grace_s:.1}s after {after}; \
                             could not kill its process group: {err}"
                        ),
                        Some(GroupKill::Unknown { .. }) => format!(
                            "{grace_s:.1}s after {after}, could not check whether the agent \
                             process was still running; sent the kill to its process group"
                        ),
                        _ => format!(
                            "the agent process had exited {grace_s:.1}s after {after}, but \
                             processes it started were still running; killed them"
                        ),
                    };
                    append_session_note(pool, session_id, "lingered", &message, events_notify)
                        .await;
                } else if report.is_none() {
                    tracing::error!(
                        session_id,
                        "grace period ran out but the session had no process group left to kill"
                    );
                }
                turn.kill_settle_deadline =
                    Some(tokio::time::Instant::now() + KILL_SETTLE);
            }
            // A process that left the group (`setsid`) can still hold the
            // pipes open after the kill, and the stream would then never
            // end. The agent itself is dead by now; stop waiting on output.
            _ = tokio::time::sleep_until(turn.kill_settle_deadline.unwrap_or(never)),
                if turn.kill_settle_deadline.is_some() =>
            {
                tracing::warn!(
                    session_id,
                    "output stream still open after killing the process group; no longer reading it"
                );
                break;
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
    //
    // A single-shot turn's group is signalled one last time first (#90).
    // The output stream closing only proves nothing still holds the
    // daemon's pipes: a background shell writing to a file, or a `nohup`ed
    // server, could still be running in the group, and with the pid retired
    // nothing could reach it again, not even cancel. Safe to signal because
    // nothing has reaped the leader yet: at worst it's a zombie, which keeps
    // its pid and so its group number. A standing session keeps the old
    // behaviour; its process ends when the conversation does.
    {
        let mut pgid = pgid.lock().await;
        if kind == SessionKind::SingleShot
            && let Some(group) = *pgid
        {
            kill_and_sweep(&tracker, group, "end of turn").await;
        }
        *pgid = None;
    }
    if kind == SessionKind::SingleShot {
        write_leftover_notes(pool, session_id, &tracker, events_notify).await;
    }

    let exit_status = handle.wait().await;
    let clean_exit = matches!(&exit_status, Ok(status) if status.success());
    if let Err(err) = &exit_status {
        tracing::error!(session_id, %err, "failed to reap subprocess");
    }

    let (final_status, end_reason) = final_run_state(
        kind,
        &turn,
        clean_exit,
        reaped,
        cancelled.load(Ordering::SeqCst),
        stopping.load(Ordering::SeqCst),
    );
    if turn.completed && !clean_exit && !turn.lingered {
        // Surprising but not actionable: the turn reported and ended, and the
        // process is gone. What it exited with doesn't change that.
        tracing::warn!(
            session_id,
            "subprocess exited non-zero after its single-shot turn had completed"
        );
    }
    let ended_at = (final_status == SessionStatus::Exited).then(Utc::now);
    // `status` and `end_reason` are set in the one statement below rather
    // than two: a watcher elsewhere (engine.rs's turn-completion watcher)
    // polls this row from a separate task and must never be able to
    // observe `status == Idle` while `end_reason` still holds a stale (or
    // absent) value from before this exit — that's exactly the gap that
    // would resurrect the ambiguity `end_reason` exists to close.
    if let Err(err) =
        sessions::update_status(pool, session_id, final_status, ended_at, end_reason).await
    {
        tracing::error!(session_id, %err, "failed to update status after drain");
    } else {
        tracing::info!(session_id, status = %final_status, ?end_reason, "session drained");
    }
}

/// What the daemon writes on stdin to a single-shot turn that ended without
/// reporting (#90).
const NUDGE_TEXT: &str = "You ended your turn without calling `report_outcome`. If your work \
     for this stage is finished, call `report_outcome` now. If you are still waiting on \
     background work, say so briefly and end your turn; you will be woken when it finishes.";

/// How long to keep reading output after killing a lingering process group,
/// before giving up on a stream something outside the group still holds.
const KILL_SETTLE: Duration = Duration::from_secs(5);

/// Timers bounding how a single-shot turn ends (#90). See [`drain_session`].
#[derive(Debug, Clone)]
pub struct TurnTimers {
    /// How long a process may keep running once its turn has ended and
    /// stdin is closed, before its group is killed.
    pub grace: Duration,
    /// How long a turn that ended without reporting may stay silent before
    /// it is nudged.
    pub nudge_after: Duration,
    /// How many nudges before a turn that never reports is closed.
    pub max_nudges: u32,
    /// How long a turn that ended without reporting may wait on its own
    /// background jobs (as the CLI reports them) before it is closed. While
    /// it waits, it is not nudged (#271).
    pub job_wait_limit: Duration,
}

impl Default for TurnTimers {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            nudge_after: Duration::from_secs(5 * 60),
            max_nudges: 3,
            job_wait_limit: Duration::from_secs(60 * 60),
        }
    }
}

/// A single-shot turn's progress towards completion, as `drain_session`
/// observes it (#90). Kept apart from the I/O so the rules read in one place.
struct SingleShotTurn {
    /// Main-agent `report_outcome` calls still waiting on their result.
    pending_reports: std::collections::HashSet<String>,
    /// A main-agent `report_outcome` call succeeded.
    reported: bool,
    /// A `result` followed a successful report: the turn is done.
    completed: bool,
    /// A clean `result` arrived with no report, and stdin is still open.
    waiting_for_report: bool,
    nudges_sent: u32,
    /// Closed after never reporting, nudges exhausted.
    gave_up: bool,
    /// The last time anything was drained from the process, or a nudge sent.
    last_event_at: tokio::time::Instant,
    /// When a process whose stdin the daemon closed gets killed if still alive.
    grace_deadline: Option<tokio::time::Instant>,
    /// Why the grace timer was started, for the note a kill leaves behind.
    grace_cause: Option<GraceCause>,
    /// Set once the grace kill fired: when to stop waiting on the stream.
    kill_settle_deadline: Option<tokio::time::Instant>,
    /// The grace kill actually signalled the group.
    lingered: bool,
    /// A `result` with `is_error` ended the turn.
    errored: bool,
    /// The CLI reported a usage limit for this turn (#92): it was cut off
    /// from outside, so the run is resumable rather than a failure of the
    /// agent's own.
    interrupted: bool,
    /// The background jobs the CLI last reported as running (#271). Each
    /// `BackgroundJobs` event replaces it.
    background_jobs: Vec<BackgroundJob>,
    /// When the turn's wait on its background jobs runs out. Fixed when the
    /// turn starts waiting on jobs; see [`SingleShotTurn::update_job_wait`].
    job_wait_deadline: Option<tokio::time::Instant>,
    /// When the wait in progress began.
    job_wait_started_at: Option<tokio::time::Instant>,
    /// Time spent in waits that have ended. The limit is a budget for the
    /// whole turn: this only rises.
    job_wait_spent: Duration,
}

impl Default for SingleShotTurn {
    fn default() -> Self {
        Self {
            pending_reports: Default::default(),
            reported: false,
            completed: false,
            waiting_for_report: false,
            nudges_sent: 0,
            gave_up: false,
            last_event_at: tokio::time::Instant::now(),
            grace_deadline: None,
            grace_cause: None,
            kill_settle_deadline: None,
            lingered: false,
            errored: false,
            interrupted: false,
            background_jobs: Vec::new(),
            job_wait_deadline: None,
            job_wait_started_at: None,
            job_wait_spent: Duration::ZERO,
        }
    }
}

/// What [`SingleShotTurn::update_job_wait`] found.
#[derive(Debug, PartialEq, Eq)]
enum JobWaitChange {
    /// A wait began; `remaining` of the turn's budget is left for it.
    Entered {
        remaining: Duration,
    },
    /// A wait began with none of the turn's budget left.
    ExhaustedOnEntry,
    NoChange,
}

/// Why `drain_session` closed a single-shot turn's stdin and started the
/// grace timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraceCause {
    /// The turn reported and ended.
    Completed,
    /// The turn ended with an error `result`.
    Errored,
    /// The turn never reported and ran out of nudges.
    NoReport,
    /// The idle reaper closed a turn that had gone silent mid-turn.
    Reaped,
}

enum TurnStep {
    Continue,
    Completed,
    EndedWithError,
    WaitingForReport,
}

impl SingleShotTurn {
    /// Folds one drained event into the turn's state.
    fn observe(&mut self, event: &AgentEvent) -> TurnStep {
        // Recorded before the guard below, not after: the CLI reports a
        // usage limit on the assistant line *and* on the `result` that ends
        // the turn (#92), and the second of those arrives once `errored` is
        // already set. Either one is enough to know the turn was cut off
        // from outside rather than by anything the agent did.
        if let AgentEvent::Interrupted { .. } = event {
            self.interrupted = true;
        }
        // Not main-agent activity, so it neither clears `waiting_for_report`
        // nor needs the guard below.
        if let AgentEvent::BackgroundJobs { running } = event {
            self.background_jobs = running.clone();
            return TurnStep::Continue;
        }
        if self.completed || self.errored || self.gave_up {
            return TurnStep::Continue;
        }
        // The agent is working again after a report-less `result`: woken by a
        // background job's notification, or answering a nudge. Until its next
        // `result` it isn't waiting, so it mustn't be nudged mid-turn by a
        // long silent tool call — a nudge there would be queued behind the
        // turn and start another one after it reports. A hung turn is still
        // bounded by the idle reaper. Only the main agent counts: a
        // background sub-agent's output doesn't mean the main agent resumed.
        if self.waiting_for_report
            && matches!(
                event,
                AgentEvent::AssistantMessage { .. }
                    | AgentEvent::Thinking { .. }
                    | AgentEvent::ToolCall { .. }
                    | AgentEvent::ToolResult { .. }
                    | AgentEvent::SessionMeta { .. }
            )
        {
            self.waiting_for_report = false;
        }
        let report_tool = chocofactory_core::mcp::qualified_report_outcome_tool_name();
        match event {
            // Only the main agent's calls: a sub-agent's arrive wrapped in
            // `AgentEvent::Subagent` and never match here.
            AgentEvent::ToolCall {
                tool_use_id, tool, ..
            } if *tool == report_tool => {
                self.pending_reports.insert(tool_use_id.clone());
                TurnStep::Continue
            }
            AgentEvent::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                if self.pending_reports.remove(tool_use_id) && !is_error {
                    self.reported = true;
                }
                TurnStep::Continue
            }
            AgentEvent::TurnCompleted { is_error: true, .. } => {
                self.errored = true;
                self.waiting_for_report = false;
                TurnStep::EndedWithError
            }
            AgentEvent::TurnCompleted {
                is_error: false, ..
            } if self.reported => {
                self.completed = true;
                self.waiting_for_report = false;
                TurnStep::Completed
            }
            AgentEvent::TurnCompleted {
                is_error: false, ..
            } => {
                self.waiting_for_report = true;
                TurnStep::WaitingForReport
            }
            _ => TurnStep::Continue,
        }
    }

    /// Starts the grace timer, unless one is already running.
    fn arm_grace(&mut self, grace: Duration, cause: GraceCause) {
        if self.grace_deadline.is_none() {
            self.grace_deadline = Some(tokio::time::Instant::now() + grace);
            self.grace_cause = Some(cause);
        }
    }

    fn should_nudge(&self) -> bool {
        self.waiting_for_report
            && !self.gave_up
            && self.grace_deadline.is_none()
            && !self.waiting_on_jobs()
    }

    /// The turn ended without reporting and the CLI says its background jobs
    /// are still running (#271): it is waiting on them, not silent.
    fn waiting_on_jobs(&self) -> bool {
        self.waiting_for_report
            && !self.gave_up
            && self.grace_deadline.is_none()
            && !self.background_jobs.is_empty()
    }

    /// The one place the job-wait deadline and budget are written.
    ///
    /// `limit` is a budget for the whole turn, summed over all its waits.
    /// Entering a wait stamps its start and sets the deadline from what is
    /// left of the budget (and only then, so later events never push it
    /// back); leaving one adds its length to the time spent.
    fn update_job_wait(&mut self, now: tokio::time::Instant, limit: Duration) -> JobWaitChange {
        if !self.waiting_on_jobs() {
            if let Some(started) = self.job_wait_started_at.take() {
                self.job_wait_spent += now.saturating_duration_since(started);
            }
            self.job_wait_deadline = None;
            return JobWaitChange::NoChange;
        }
        if self.job_wait_started_at.is_some() {
            return JobWaitChange::NoChange;
        }
        self.job_wait_started_at = Some(now);
        let remaining = limit.saturating_sub(self.job_wait_spent);
        if remaining.is_zero() {
            self.job_wait_deadline = None;
            return JobWaitChange::ExhaustedOnEntry;
        }
        // A remaining time too large for the clock means no bound at all.
        self.job_wait_deadline = Some(
            now.checked_add(remaining)
                .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365)),
        );
        JobWaitChange::Entered { remaining }
    }

    /// The job-wait deadline has passed while the turn is still waiting.
    fn job_wait_expired(&self, now: tokio::time::Instant) -> bool {
        self.waiting_on_jobs()
            && self
                .job_wait_deadline
                .is_some_and(|deadline| now >= deadline)
    }
}

/// The limit as a person would write it: "60 min" for whole minutes.
fn describe_limit(limit: Duration) -> String {
    let secs = limit.as_secs();
    if limit.subsec_nanos() == 0 && secs > 0 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{limit:?}")
    }
}

/// Records that a turn has started waiting on its background jobs (#271).
async fn note_job_wait_entered(
    pool: &SqlitePool,
    session_id: &str,
    jobs: &[BackgroundJob],
    limit: Duration,
    remaining: Duration,
    events_notify: &Notify,
) {
    let deadline = chrono::Duration::from_std(remaining)
        .ok()
        .and_then(|limit| Utc::now().checked_add_signed(limit))
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "never".to_string());
    append_session_note(
        pool,
        session_id,
        "job_wait",
        &format!(
            "the turn ended without reporting while {} background job(s) run ({}); not nudging \
             until they finish, closing it at {deadline} (the {} limit is a total for the turn)",
            jobs.len(),
            describe_jobs(jobs),
            describe_limit(limit)
        ),
        events_notify,
    )
    .await;
}

/// Closes a turn whose job-wait limit ran out, the way an exhausted nudge
/// count closes one: `gave_up`, a `no_report` note, stdin closed, grace armed.
async fn close_after_job_wait(
    pool: &SqlitePool,
    session_id: &str,
    handle: &mut AgentHandle,
    turn: &mut SingleShotTurn,
    turn_timers: &TurnTimers,
    already_used_up: bool,
    events_notify: &Notify,
) {
    // The note is built before `gave_up` makes the turn stop waiting on jobs.
    let when = if already_used_up {
        "had already been used up by its earlier waits"
    } else {
        "ran out"
    };
    let message = format!(
        "the turn never called report_outcome and was still waiting on {} background job(s) \
         ({}) when the turn's job-wait limit of {} (a total per turn) {when}; closing it",
        turn.background_jobs.len(),
        describe_jobs(&turn.background_jobs),
        describe_limit(turn_timers.job_wait_limit)
    );
    turn.gave_up = true;
    turn.job_wait_deadline = None;
    tracing::warn!(
        session_id,
        "single-shot turn still waiting on background jobs at the limit; closing it"
    );
    append_session_note(pool, session_id, "no_report", &message, events_notify).await;
    handle.close_stdin();
    turn.arm_grace(turn_timers.grace, GraceCause::NoReport);
}

/// The run's final `status` and `end_reason` once its process is gone.
///
/// `Cancelled` is checked first and, unlike `Reaped`, without requiring a
/// clean exit — a SIGKILLed process exits by signal, so predicating it on
/// `clean_exit` would record every cancel as an anonymous crash. A human
/// asked for it, which is the more useful thing for `choco task status` to
/// say. It is also honest when a turn happened to finish in the instant
/// before the signal landed; the engine's `tasks.status == "cancelled"`
/// guard, not this row, is what stops a cancelled task from advancing.
///
/// `DaemonStopped` (the daemon shutting down) ranks just below `Cancelled`.
/// A single-shot turn that had already completed keeps its ordinary result;
/// a standing session goes to `idle` with it so a chat stays resumable.
///
/// For a single-shot turn (#90), only a completed turn is `idle`. After
/// `Cancelled`, the most specific explanation wins: a turn that never
/// reported is `NoReport`, even if its process then had to be killed; one the
/// idle reaper closed mid-turn is `Reaped`, whether it exited cleanly
/// (`idle`, as before #90) or had to be killed (`exited`); one that
/// completed or errored but whose process had to be killed is `Lingered`.
/// Anything else that exits without having completed is `exited`: cleanly
/// means it never reported (`NoReport`), otherwise it crashed (no reason).
fn final_run_state(
    kind: SessionKind,
    turn: &SingleShotTurn,
    clean_exit: bool,
    reaped: bool,
    cancelled: bool,
    stopping: bool,
) -> (SessionStatus, Option<SessionEndReason>) {
    use SessionEndReason::{Cancelled, DaemonStopped, Interrupted, Lingered, NoReport, Reaped};
    use SessionStatus::{Exited, Idle};
    match kind {
        // A clean exit (reaper-driven close, or a one-shot process finishing on
        // its own) goes to `idle`, ready to resume. A crash, auth failure, or
        // signal kill goes to `exited` instead — otherwise a deterministic
        // failure would just get resumed into the same crash forever.
        SessionKind::Standing => {
            let status = if clean_exit { Idle } else { Exited };
            if cancelled {
                return (status, Some(Cancelled));
            }
            if stopping {
                // Idle whatever the exit looked like: the daemon killed it,
                // and a chat stays resumable by message.
                return (Idle, Some(DaemonStopped));
            }
            let reason = if clean_exit && reaped {
                Some(Reaped)
            } else {
                None
            };
            (status, reason)
        }
        SessionKind::SingleShot => {
            if cancelled {
                let status = if clean_exit && turn.completed {
                    Idle
                } else {
                    Exited
                };
                (status, Some(Cancelled))
            } else if stopping && !turn.completed {
                (Exited, Some(DaemonStopped))
            } else if turn.gave_up {
                (Exited, Some(NoReport))
            } else if reaped && !turn.completed {
                // The idle reaper closed a stalled turn, which the engine's
                // watcher parks on whatever the status.
                let status = if clean_exit { Idle } else { Exited };
                (status, Some(Reaped))
            } else if turn.lingered {
                (Exited, Some(Lingered))
            } else if turn.completed {
                (Idle, None)
            } else if clean_exit && !turn.errored {
                // Ahead of the interruption arm below on purpose: a turn
                // that saw a limit, carried on, and then ended its turn
                // cleanly without reporting was not stopped by the limit —
                // it is the #90 case, and resuming it would resume a turn
                // that has nothing to say.
                (Exited, Some(NoReport))
            } else if turn.interrupted {
                // #92. Last of the named reasons, because every one above
                // describes something more specific — what the daemon did
                // to this run, or how the turn itself ended. What this arm
                // claims is only what they all leave anonymous today: a
                // turn that was working and was cut off by a usage limit,
                // which `retry` can resume rather than restart.
                (Exited, Some(Interrupted))
            } else {
                (Exited, None)
            }
        }
    }
}

/// How many times a sweep rescans and kills, and the pause between rounds.
const SWEEP_ROUNDS: usize = 10;
const SWEEP_INTERVAL: Duration = Duration::from_millis(50);

/// What a turn's kills and sweeps found, collected from every site that
/// signals the agent's group (`cancel`, `shutdown`, the grace branch, the
/// tail) and reported once by `drain_session`.
struct LeftoverTracker {
    marker: String,
    kind: SessionKind,
    reader: ProcReader,
    state: std::sync::Mutex<TrackerState>,
}

#[derive(Default)]
struct TrackerState {
    /// Identities of processes proven to descend from the agent.
    recorded: std::collections::HashSet<Identity>,
    killed: Vec<(i32, String)>,
    survivors: Vec<(i32, String)>,
    scan_failures: Vec<String>,
}

/// What [`kill_and_sweep`] did at one site.
struct KillReport {
    group: GroupKill,
    /// Live processes the sweeps of this call killed.
    sweep_killed: usize,
}

/// Whether a grace-site kill means the agent was lingering: unset only when
/// the group had nothing alive and the sweep killed nothing. `Failed` and
/// `Unknown` err towards parking the task, as the code always has.
fn lingered_decision(group: &GroupKill, sweep_killed: usize) -> bool {
    !matches!(group, GroupKill::NothingAlive) || sweep_killed > 0
}

fn describe_group_kill(group: &GroupKill) -> String {
    match group {
        GroupKill::Killed(n) => format!("killed {n} live member(s)"),
        GroupKill::NothingAlive => "nothing alive".to_string(),
        GroupKill::Failed(err) => format!("failed: {err}"),
        GroupKill::Unknown {
            scan_error,
            signal_result,
        } => format!("unknown (scan failed: {scan_error}; signal: {signal_result:?})"),
    }
}

fn lingered_after(report: &KillReport) -> bool {
    lingered_decision(&report.group, report.sweep_killed)
}

impl TrackerState {
    /// Records a process that could not be killed, once per pid.
    fn add_survivor(&mut self, pid: i32, comm: String) {
        let listed = |list: &[(i32, String)]| list.iter().any(|(p, _)| *p == pid);
        if !listed(&self.survivors) && !listed(&self.killed) {
            self.survivors.push((pid, comm));
        }
    }
}

impl LeftoverTracker {
    fn new(marker: String, kind: SessionKind, reader: ProcReader) -> Self {
        Self {
            marker,
            kind,
            reader,
            state: Default::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads the table off the runtime thread.
    async fn scan(&self, with_marker: bool) -> std::io::Result<Vec<ProcEntry>> {
        let reader = Arc::clone(&self.reader);
        let marker = with_marker.then(|| self.marker.clone());
        tokio::task::spawn_blocking(move || reader(marker.as_deref()))
            .await
            .map_err(std::io::Error::other)?
    }

    /// A failed scan: logged where it happens, and kept for the
    /// `leftovers_unchecked` note.
    fn note_scan_failure(&self, site: &str, err: &std::io::Error) {
        tracing::error!(site, %err, "could not read the process table; leftovers were not checked");
        self.state().scan_failures.push(format!("{site}: {err}"));
    }

    fn record_table(&self, table: &[ProcEntry], agent: i32) {
        let found = proc_table::descendants(table, agent);
        let mut state = self.state();
        for entry in table.iter().filter(|e| found.contains(&e.pid)) {
            state.recorded.insert((entry.pid, entry.start));
        }
    }

    /// Scans (no environments) and records the agent's current descendants.
    async fn record_descendants(&self, agent: u32, site: &str) {
        let Ok(agent) = i32::try_from(agent) else {
            return;
        };
        match self.scan(false).await {
            Ok(table) => self.record_table(&table, agent),
            Err(err) => self.note_scan_failure(site, &err),
        }
    }

    /// Scans with the marker, SIGKILLs everything owned by pid, and repeats
    /// (bounded) to catch children forked between a scan and its kill.
    /// Returns how many live processes it killed.
    async fn sweep(&self, agent: i32, site: &str) -> usize {
        // SAFETY: both only read process state.
        let (daemon_sid, uid) = unsafe { (libc::getsid(0), libc::geteuid()) };
        let mut failed = std::collections::HashSet::new();
        let mut killed_here = 0usize;
        for round in 0..=SWEEP_ROUNDS {
            let table = match self.scan(true).await {
                Ok(table) => table,
                Err(err) => {
                    self.note_scan_failure(site, &err);
                    return killed_here;
                }
            };
            let recorded = self.state().recorded.clone();
            let owned: Vec<i32> = proc_table::owned_pids(
                &table,
                &OwnershipInput {
                    agent: Some(agent),
                    recorded: &recorded,
                    daemon_pid: std::process::id() as i32,
                    daemon_sid,
                    uid,
                },
            )
            .into_iter()
            .filter(|pid| !failed.contains(pid))
            .collect();
            let comm = |pid: i32| {
                table
                    .iter()
                    .find(|e| e.pid == pid)
                    .map(|e| e.comm.clone())
                    .unwrap_or_default()
            };
            if owned.is_empty() {
                break;
            }
            if round == SWEEP_ROUNDS {
                tracing::warn!(
                    site,
                    count = owned.len(),
                    "processes the turn started survived the sweep"
                );
                let mut state = self.state();
                for pid in owned {
                    state.add_survivor(pid, comm(pid));
                }
                break;
            }
            // Pid reuse between the scan and this kill is not guarded beyond
            // the scan: both kernels allocate pids sequentially, so it would
            // take a full wraparound inside this window. Accepted cost.
            for pid in owned {
                match proc_table::kill_pid(pid) {
                    proc_table::PidKill::Killed => {
                        let mut state = self.state();
                        // Killed now: no longer a survivor of an earlier site.
                        state.survivors.retain(|(p, _)| *p != pid);
                        if !state.killed.iter().any(|(p, _)| *p == pid) {
                            state.killed.push((pid, comm(pid)));
                            killed_here += 1;
                        }
                    }
                    proc_table::PidKill::Gone => {}
                    proc_table::PidKill::Failed(err) => {
                        tracing::error!(site, pid, %err, "could not kill a leftover process");
                        failed.insert(pid);
                        self.state().add_survivor(pid, comm(pid));
                    }
                }
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
        if killed_here > 0 {
            tracing::warn!(
                site,
                killed = killed_here,
                "killed processes left behind by the turn"
            );
        }
        killed_here
    }
}

/// Records the agent's descendants, SIGKILLs its process group, then sweeps
/// for everything else the turn started. The one way the daemon signals an
/// agent's group, so no site can skip a step.
///
/// The caller holds the session's `pgid` lock for the whole call, so the
/// agent stays unreaped (its pid valid, and a descendant link through it
/// sound) until the sweep is over. The one scan before the kill serves both
/// as the record and as the classification of the kill.
async fn kill_and_sweep(tracker: &LeftoverTracker, pgid: u32, site: &str) -> KillReport {
    let agent = pgid as i32;
    let scan = tracker.scan(false).await;
    let live = match &scan {
        Ok(table) => Ok(crate::shell::live_members(table, agent)),
        Err(err) => Err(std::io::Error::new(err.kind(), err.to_string())),
    };
    if tracker.kind == SessionKind::Standing {
        // Chat sessions only get the group kill, as before.
        return KillReport {
            group: crate::shell::kill_group_with(pgid, live),
            sweep_killed: 0,
        };
    }
    match &scan {
        Ok(table) => tracker.record_table(table, agent),
        Err(err) => tracker.note_scan_failure(site, err),
    }
    let group = crate::shell::kill_group_with(pgid, live);
    if let GroupKill::Failed(err) = &group {
        tracker
            .state()
            .add_survivor(agent, format!("process group ({err})"));
    }
    let sweep_killed = tracker.sweep(agent, site).await;
    tracing::debug!(site, group = %describe_group_kill(&group), sweep_killed, "killed the agent's group and swept");
    KillReport {
        group,
        sweep_killed,
    }
}

/// Writes the `leftovers_killed` and `leftovers_unchecked` notes, once, at the
/// end of the turn.
async fn write_leftover_notes(
    pool: &SqlitePool,
    session_id: &str,
    tracker: &LeftoverTracker,
    events_notify: &Notify,
) {
    let (killed, survivors, failures) = {
        let mut state = tracker.state();
        (
            std::mem::take(&mut state.killed),
            std::mem::take(&mut state.survivors),
            std::mem::take(&mut state.scan_failures),
        )
    };
    let list = |items: &[(i32, String)]| {
        items
            .iter()
            .map(|(pid, comm)| format!("{pid} ({comm})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !killed.is_empty() || !survivors.is_empty() {
        let mut parts = Vec::new();
        if !killed.is_empty() {
            parts.push(format!(
                "killed {} process(es) the turn left running: {}",
                killed.len(),
                list(&killed)
            ));
        }
        if !survivors.is_empty() {
            parts.push(format!(
                "{} could not be killed and may still be running: {}",
                survivors.len(),
                list(&survivors)
            ));
        }
        let message = parts.join("; ");
        append_session_note(
            pool,
            session_id,
            "leftovers_killed",
            &message,
            events_notify,
        )
        .await;
    }
    if !failures.is_empty() {
        append_session_note(
            pool,
            session_id,
            "leftovers_unchecked",
            &format!(
                "the process table could not be read, so what the turn left running could not be \
                 checked (the agent's process group was still killed): {}",
                failures.join("; ")
            ),
            events_notify,
        )
        .await;
    }
}

/// Records one of the daemon's own interventions on a run's timeline (#90).
/// Best-effort, like every other event append here: the intervention itself
/// has already happened, and the run's `end_reason` still records the
/// outcome if this write is lost.
async fn append_session_note(
    pool: &SqlitePool,
    session_id: &str,
    kind: &str,
    message: &str,
    events_notify: &Notify,
) {
    match events::append(
        pool,
        session_id,
        EventType::SessionNote,
        serde_json::json!({ "kind": kind, "message": message }),
    )
    .await
    {
        Ok(_) => events_notify.notify_waiters(),
        Err(err) => tracing::error!(session_id, kind, %err, "failed to record a session note"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration as StdDuration;

    use serde_json::json;

    use super::*;
    use crate::adapter::{AgentAdapter, ClaudeAdapter};
    use crate::db::{connect_in_memory, events, projects, sessions, tasks};

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    async fn seed_session(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        let task_id = tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "chat",
                title: "T",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
            },
        )
        .await
        .unwrap()
        .id;
        sessions::create(
            pool,
            sessions::NewSession {
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
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: crate::adapter::Isolation::InheritOperatorConfig,
        }
    }

    /// A single-shot stage's config: one allowed outcome, which is what
    /// makes the adapter tell the turn to report, and the fixtures report.
    fn single_shot_role_config() -> RoleConfig {
        RoleConfig {
            report_outcomes: vec!["done".to_string()],
            report_sections: Vec::new(),
            ..role_config()
        }
    }

    /// Event persistence happens on a spawned background task, so tests
    /// poll with a short bounded retry instead of sleeping a fixed time.
    async fn wait_until_events_len(
        pool: &SqlitePool,
        session_id: &str,
        expected: usize,
    ) -> Vec<chocofactory_core::models::Event> {
        crate::test_support::wait_until(
            &format!("{expected} events on session {session_id}"),
            || async {
                let stored = events::list_for_session(pool, session_id).await.unwrap();
                if stored.len() >= expected {
                    Ok(stored)
                } else {
                    Err(format!("{} events", stored.len()))
                }
            },
        )
        .await
    }

    async fn wait_until_status(pool: &SqlitePool, session_id: &str, expected: SessionStatus) {
        crate::test_support::wait_until(
            &format!("status {expected:?} on session {session_id}"),
            || async {
                let run = sessions::get(pool, session_id).await.unwrap().unwrap();
                if run.status == expected {
                    Ok(())
                } else {
                    Err(format!("status {:?}", run.status))
                }
            },
        )
        .await
    }

    #[tokio::test]
    async fn a_crashed_subprocess_is_recorded_as_exited_not_idle() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_crash.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();

        // A non-zero exit should land the run in `exited`, not the
        // `idle` (resumable) state a clean reaper-driven close gets.
        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert!(run.ended_at.is_some());
    }

    #[tokio::test]
    async fn start_spawns_a_session_and_drains_its_events() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &session_id, 2).await;
        assert_eq!(stored[1].payload["text"], "echo:hello");

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
        assert!(run.adapter_session_id.is_some());
    }

    #[tokio::test]
    async fn a_sub_agent_result_records_no_usage_row() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_subagent_result.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();

        // The sub-agent result and the top-level one both become events;
        // only the top-level one may leave a usage row.
        crate::test_support::wait_until("both result events", || async {
            let stored = events::list_for_session(&pool, &session_id).await.unwrap();
            let n = stored
                .iter()
                .filter(|e| e.event_type == chocofactory_core::models::EventType::TurnCompleted)
                .count();
            if n >= 2 {
                Ok(())
            } else {
                Err(format!("{n} turn events"))
            }
        })
        .await;
        let rows = crate::db::usage::list_rows_for_task(&pool, &task_of(&pool, &session_id).await)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cost_usd, Some(0.01));
    }

    async fn task_of(pool: &SqlitePool, session_id: &str) -> String {
        sessions::get(pool, session_id)
            .await
            .unwrap()
            .unwrap()
            .task_id
    }

    #[tokio::test]
    async fn send_message_forwards_to_an_active_in_memory_session() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        manager
            .send_message(&session_id, "again", &role_config())
            .await
            .unwrap();

        // SessionMeta, AssistantMessage("echo:hello"), TurnCompleted (#70:
        // fake_claude.py's `result` line, no longer discarded),
        // AssistantMessage("echo:again").
        let stored = wait_until_events_len(&pool, &session_id, 4).await;
        assert_eq!(stored[3].payload["text"], "echo:again");
    }

    #[tokio::test]
    async fn send_message_resumes_from_a_persisted_adapter_session_id_when_not_active_in_memory() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        sessions::set_adapter_session_id(&pool, &session_id, "fixed-session-id")
            .await
            .unwrap();
        sessions::update_status(&pool, &session_id, SessionStatus::Idle, None, None)
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .send_message(&session_id, "hello again", &role_config())
            .await
            .unwrap();

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
        assert_eq!(run.adapter_session_id.as_deref(), Some("fixed-session-id"));

        let stored = wait_until_events_len(&pool, &session_id, 2).await;
        assert_eq!(stored[1].payload["text"], "echo:hello again");
    }

    #[tokio::test]
    async fn a_send_queued_behind_a_stale_reaper_close_is_not_dropped() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // A real timeout, so last_activity looks fresh once the queued
        // Close below is actually dequeued and re-checked.
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        // Simulate the reaper enqueueing a Close based on a stale read of
        // last_activity, taken before the send_message below bumps it -
        // reproduces the ordering from the review finding without
        // depending on real scheduler timing.
        {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&session_id) else {
                panic!("session should be live");
            };
            session.cmd_tx.send(Command::Close).unwrap();
        }

        manager
            .send_message(&session_id, "again", &role_config())
            .await
            .unwrap();

        // SessionMeta, AssistantMessage("echo:hello"), TurnCompleted (#70:
        // fake_claude.py's `result` line, no longer discarded),
        // AssistantMessage("echo:again").
        let stored = wait_until_events_len(&pool, &session_id, 4).await;
        assert_eq!(stored[3].payload["text"], "echo:again");
    }

    #[tokio::test]
    async fn send_message_rejects_an_exited_session() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        sessions::update_status(
            &pool,
            &session_id,
            SessionStatus::Exited,
            Some(Utc::now()),
            None,
        )
        .await
        .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        let err = manager
            .send_message(&session_id, "hello", &role_config())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::NotResumable(SessionStatus::Exited)
        ));
    }

    #[tokio::test]
    async fn send_message_rejects_a_concurrent_establish_for_the_same_session() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        sessions::set_adapter_session_id(&pool, &session_id, "fixed-session-id")
            .await
            .unwrap();
        sessions::update_status(&pool, &session_id, SessionStatus::Idle, None, None)
            .await
            .unwrap();

        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        // Simulate another in-flight call that already claimed the slot
        // between send_message's optimistic map check and its DB read.
        manager.reserve(&session_id).await.unwrap();

        let err = manager
            .send_message(&session_id, "hello", &role_config())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::AlreadyStarting));
    }

    #[tokio::test]
    async fn send_message_resumes_a_session_the_reaper_previously_idled() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: the reaper closes the session on its first pass.
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;
        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;

        manager
            .send_message(&session_id, "again", &role_config())
            .await
            .unwrap();

        // Turn 1: SessionMeta, AssistantMessage("echo:hello"), TurnCompleted
        // (#70: fake_claude.py's `result` line, no longer discarded). Then
        // the resumed process is a fresh subprocess too, so it emits its
        // own SessionMeta (event 3) before the AssistantMessage (event 4).
        let stored = wait_until_events_len(&pool, &session_id, 5).await;
        assert_eq!(stored[4].payload["text"], "echo:again");
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
    }

    #[tokio::test]
    async fn idle_reaper_closes_sessions_past_the_idle_timeout() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: any session is immediately overdue.
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;

        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;

        // Regression test for the review on PR #35: a reaper-driven clean
        // exit must be distinguishable from a turn that finished on its
        // own, since both land on `Idle` — `end_reason` is what the
        // workflow engine's completion watcher relies on to tell them
        // apart.
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::Reaped));
    }

    #[tokio::test]
    async fn a_session_that_finishes_on_its_own_has_no_end_reason() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_oneshot.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();

        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, None);
    }

    /// Regression test for #70: `fake_claude.py` never exits on its own —
    /// exactly the real `claude --input-format stream-json` CLI's shape —
    /// so this hangs (times out waiting for `Idle`) without the fix, which
    /// used to wait for `handle.wait()` to resolve. A `SingleShot` session
    /// must instead complete the moment the fixture's `result` line
    /// arrives, and close stdin itself so the process actually exits.
    #[tokio::test]
    async fn a_single_shot_session_completes_and_closes_stdin_against_a_stay_open_fixture() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();

        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(
            run.end_reason, None,
            "a real completion must not look reaped"
        );

        // SessionMeta, the report_outcome call and its result (#90),
        // AssistantMessage, TurnCompleted.
        let stored = wait_until_events_len(&pool, &session_id, 5).await;
        assert_eq!(
            stored[4].event_type,
            chocofactory_core::models::EventType::TurnCompleted
        );
    }

    #[tokio::test]
    async fn idle_reaper_leaves_sessions_within_the_idle_timeout_active() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

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
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
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
        crate::test_support::wait_until(&format!("pid {pid} to exit"), || async {
            if !process_alive(pid) {
                Ok(())
            } else {
                Err(format!("pid {pid} still alive"))
            }
        })
        .await
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
                "#!/bin/sh\nCHOCO_TEST_HEARTBEAT='{}' CHOCO_TEST_CHILD_PID='{}' CHOCO_TEST_AGENT_PID='{}' exec '{}' \"$@\"\n",
                heartbeat.display(),
                child_pid.display(),
                dir.join("agent.pid").display(),
                fixture_binary("fake_claude_spawns_child.py"),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        (wrapper.display().to_string(), heartbeat, child_pid)
    }

    async fn read_pid_when_written(path: &std::path::Path) -> u32 {
        crate::test_support::wait_until(
            &format!("the fixture to write its child pid to {}", path.display()),
            || async {
                match std::fs::read_to_string(path) {
                    Ok(text) => text
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| format!("unparseable pid file contents {text:?}")),
                    Err(e) => Err(format!("cannot read pid file: {e}")),
                }
            },
        )
        .await
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("chocofactoryd-cancel-{}", uuid::Uuid::new_v4()));
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
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        manager.cancel(&session_id).await.unwrap();

        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        // The distinction that matters: a SIGKILLed process exits
        // non-zero, which is indistinguishable from a crash by `status`
        // alone. `end_reason` is what tells an operator their cancel is
        // what stopped it.
        assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled));
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
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();

        let child_pid = read_pid_when_written(&child_pid_path).await;
        assert!(
            process_alive(child_pid),
            "the fixture's child should be running before cancel"
        );

        manager.cancel(&session_id).await.unwrap();

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

        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled));
    }

    /// Graceful shutdown (#84): a live single-shot turn's whole group is
    /// killed and the run records `DaemonStopped`, not an anonymous crash.
    #[tokio::test]
    async fn shutdown_kills_a_live_turn_and_its_children_and_records_daemon_stopped() {
        let dir = TempDir::new();
        let (binary, heartbeat, child_pid_path) = spawns_child_binary(&dir.0);

        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();
        let child_pid = read_pid_when_written(&child_pid_path).await;
        let agent_pid = read_pid_when_written(&dir.0.join("agent.pid")).await;
        assert!(process_alive(child_pid) && process_alive(agent_pid));

        manager.shutdown(StdDuration::from_secs(10)).await;

        wait_until_gone(agent_pid).await;
        wait_until_gone(child_pid).await;
        let after_kill = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        let later = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert_eq!(after_kill, later, "the heartbeat should have stopped");

        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::DaemonStopped));
        assert!(run.ended_at.is_some());
    }

    /// The other half of the start/shutdown race guard: a start that
    /// reserved its slot and spawned before `shutdown` set the flag, but
    /// reaches the `Live` insert after the snapshot, is killed by the
    /// re-check in `spawn_drain`.
    #[tokio::test]
    async fn a_session_that_slips_past_the_shutdown_snapshot_is_killed_on_insert() {
        let dir = TempDir::new();
        let (binary, _heartbeat, child_pid_path) = spawns_child_binary(&dir.0);
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager.reserve(&session_id).await.unwrap();
        let handle = manager
            .registry()
            .lookup(None, "claude")
            .unwrap()
            .start("go", &single_shot_role_config())
            .unwrap();
        let child_pid = read_pid_when_written(&child_pid_path).await;
        let agent_pid = read_pid_when_written(&dir.0.join("agent.pid")).await;
        // `shutdown` has already run its snapshot (it skipped the
        // Establishing slot); only the flag is visible to this start.
        manager.shutting_down.store(true, Ordering::SeqCst);
        manager
            .spawn_drain(session_id.clone(), handle, SessionKind::SingleShot)
            .await;

        wait_until_gone(agent_pid).await;
        wait_until_gone(child_pid).await;
        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::DaemonStopped));
    }

    #[tokio::test]
    async fn nothing_starts_after_shutdown() {
        let dir = TempDir::new();
        let (binary, _heartbeat, child_pid_path) = spawns_child_binary(&dir.0);
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        manager.shutdown(StdDuration::from_millis(100)).await;

        let err = manager
            .start(
                &session_id,
                "claude",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::ShuttingDown), "{err:?}");
        let err = manager
            .resume(
                &session_id,
                "claude",
                "adapter-session",
                "go",
                &role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::ShuttingDown), "{err:?}");
        assert_eq!(err.to_string(), "the daemon is shutting down");
        // Nothing was spawned, and no slot was left reserved.
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(!child_pid_path.exists());
        assert!(!dir.0.join("agent.pid").exists());
        assert!(manager.sessions.lock().await.is_empty());
    }

    /// A turn that ignores stdin entirely is exactly what the idle
    /// reaper's `close_stdin` cannot stop, so cancel must not depend on
    /// the subprocess cooperating.
    #[tokio::test]
    async fn cancel_stops_a_turn_that_never_reads_its_stdin() {
        let dir = TempDir::new();
        let (binary, _heartbeat, child_pid_path) = spawns_child_binary(&dir.0);

        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        read_pid_when_written(&child_pid_path).await;

        manager.cancel(&session_id).await.unwrap();

        wait_until_status(&pool, &session_id, SessionStatus::Exited).await;
    }

    /// Cancelling a run with no live process is the state cancel is trying
    /// to reach, so it succeeds rather than erroring — otherwise the engine
    /// would have to special-case every `human_gate`/`poll`/`terminal`
    /// stage and every already-exited run before daring to call this.
    #[tokio::test]
    async fn cancel_is_a_no_op_for_a_run_with_no_live_session() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager.cancel(&session_id).await.unwrap();
        manager.cancel("no-such-run").await.unwrap();
    }

    /// A session mid-spawn is the one case that must *not* report success:
    /// the process doesn't exist yet and isn't reachable from the map, so
    /// answering `Ok` would tell an operator an agent was stopped while it
    /// was in fact just starting.
    #[tokio::test]
    async fn cancel_rejects_a_run_whose_session_is_still_being_established() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager.reserve(&session_id).await.unwrap();

        let err = manager.cancel(&session_id).await.unwrap_err();
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
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        // Zero timeout: the reaper closes this session on its first pass.
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 2).await;

        // The flag without the kill: stands in for a cancel whose SIGKILL
        // lands just after the process has already wound down on its own.
        {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&session_id) else {
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
        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(
            run.end_reason,
            Some(SessionEndReason::Cancelled),
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
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_oneshot.py",
        )));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        // Grab the shared pgid handle while the session is live, so it can
        // still be inspected after the map slot is gone.
        manager
            .start(
                &session_id,
                "claude",
                "hello",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
        let pgid = {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&session_id) else {
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
        wait_until_status(&pool, &session_id, SessionStatus::Idle).await;

        assert!(
            pgid.lock().await.is_none(),
            "the pgid must be cleared before the child is reaped, or a later \
             cancel could signal a reused pid"
        );
    }

    // ---- turn completion (#90) ----

    /// Timers short enough for tests: a nudge after 150ms of silence, a
    /// 400ms grace period.
    fn fast_timers(max_nudges: u32) -> TurnTimers {
        TurnTimers {
            grace: StdDuration::from_millis(400),
            nudge_after: StdDuration::from_millis(150),
            max_nudges,
            job_wait_limit: StdDuration::from_secs(60),
        }
    }

    /// A `fake_claude_script.py` wrapper following `steps`. A generated
    /// script rather than `set_var`, as in `spawns_child_binary`.
    fn script_binary(dir: &std::path::Path, steps: serde_json::Value) -> String {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("script.json");
        std::fs::write(&script, steps.to_string()).unwrap();
        let wrapper = dir.join("fake-claude-script");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nFAKE_CLAUDE_SCRIPT='{}' exec '{}' \"$@\"\n",
                script.display(),
                fixture_binary("fake_claude_script.py"),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        wrapper.display().to_string()
    }

    async fn start_single_shot(
        binary: String,
        timers: TurnTimers,
    ) -> (SqlitePool, String, Arc<SessionManager>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
            timers,
        );
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();
        (pool, session_id, manager)
    }

    async fn wait_until_final(
        pool: &SqlitePool,
        session_id: &str,
    ) -> chocofactory_core::models::Session {
        crate::test_support::wait_until(
            &format!("session {session_id} to leave active"),
            || async {
                let run = sessions::get(pool, session_id).await.unwrap().unwrap();
                if run.status != SessionStatus::Active {
                    Ok(run)
                } else {
                    Err(format!("status {:?}", run.status))
                }
            },
        )
        .await
    }

    async fn session_notes(pool: &SqlitePool, session_id: &str) -> Vec<String> {
        events::list_for_session(pool, session_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionNote)
            .map(|e| e.payload["kind"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// The #88 shape: the turn ends (a `result`) without reporting, because
    /// it's waiting on something. It must not count as done; the run stays
    /// active, and once the turn reports and ends again it completes.
    #[tokio::test]
    async fn a_result_without_a_report_leaves_the_turn_open_until_it_reports() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "started the work in the background"},
                {"op": "result"},
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "all done"},
                {"op": "result"},
            ]),
        );
        // Remaining timing dependency (#98): the fixture answers the first
        // nudge within `nudge_after` (2 s), or a second nudge is recorded.
        let timers = TurnTimers {
            nudge_after: crate::test_support::RESPONSE_MARGIN,
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        // The only thing that could have delivered the second turn is the
        // daemon's nudge.
        assert_eq!(session_notes(&pool, &session_id).await, vec!["nudge"]);
    }

    #[tokio::test]
    async fn a_turn_that_never_reports_is_nudged_then_closed_as_no_report() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "hmm"},
                {"op": "result"},
                {"op": "answer_every_turn", "text": "still thinking"},
            ]),
        );
        // Each nudge must be answered before the next is due, and the process
        // must exit on stdin EOF without the grace timer killing it (#98).
        let timers = TurnTimers {
            nudge_after: crate::test_support::RESPONSE_MARGIN,
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(2)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        assert_eq!(
            session_notes(&pool, &session_id).await,
            vec!["nudge", "nudge", "no_report"]
        );
    }

    // ---- background-job wait (#271) ----

    fn one_job() -> serde_json::Value {
        json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [
            {"task_id": "job1", "task_type": "local_bash", "description": "hung gate"}
        ]})
    }

    fn no_jobs() -> serde_json::Value {
        json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []})
    }

    async fn note_events(
        pool: &SqlitePool,
        session_id: &str,
    ) -> Vec<chocofactory_core::models::Event> {
        events::list_for_session(pool, session_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionNote)
            .collect()
    }

    async fn wait_for_note(pool: &SqlitePool, session_id: &str, kind: &str) {
        crate::test_support::wait_until(&format!("a {kind} note"), || async {
            if session_notes(pool, session_id)
                .await
                .iter()
                .any(|k| k == kind)
            {
                Ok(())
            } else {
                Err("not yet".to_string())
            }
        })
        .await
    }

    #[test]
    fn background_jobs_do_not_end_the_wait_for_a_report_but_stop_nudges() {
        let jobs = |n: usize| AgentEvent::BackgroundJobs {
            running: (0..n)
                .map(|i| BackgroundJob {
                    id: format!("j{i}"),
                    kind: "local_bash".into(),
                    description: "d".into(),
                })
                .collect(),
        };
        let mut turn = turn_with(|t| t.waiting_for_report = true);
        assert!(turn.should_nudge());
        turn.observe(&jobs(1));
        assert!(turn.waiting_for_report);
        assert!(turn.waiting_on_jobs());
        assert!(!turn.should_nudge());
        turn.observe(&jobs(0));
        assert!(turn.waiting_for_report);
        assert!(!turn.waiting_on_jobs());
        assert!(turn.should_nudge());
    }

    #[test]
    fn background_jobs_mid_turn_are_not_a_wait() {
        let mut turn = SingleShotTurn::default();
        turn.observe(&AgentEvent::BackgroundJobs {
            running: vec![BackgroundJob {
                id: "j".into(),
                kind: "k".into(),
                description: "d".into(),
            }],
        });
        assert!(!turn.waiting_for_report);
        assert!(!turn.should_nudge());
        assert!(!turn.waiting_on_jobs());
    }

    fn a_job() -> BackgroundJob {
        BackgroundJob {
            id: "j".into(),
            kind: "k".into(),
            description: "d".into(),
        }
    }

    fn waiting_turn() -> SingleShotTurn {
        turn_with(|t| {
            t.waiting_for_report = true;
            t.background_jobs = vec![a_job()];
        })
    }

    #[test]
    fn update_job_wait_fixes_the_deadline_when_the_wait_starts() {
        let limit = StdDuration::from_secs(10);
        let mut turn = waiting_turn();
        let now = tokio::time::Instant::now();
        // The first entry gets the full limit.
        assert_eq!(
            turn.update_job_wait(now, limit),
            JobWaitChange::Entered { remaining: limit }
        );
        assert_eq!(turn.job_wait_deadline, Some(now + limit));
        let later = now + StdDuration::from_millis(100);
        assert_eq!(turn.update_job_wait(later, limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_deadline, Some(now + limit));
        assert!(!turn.job_wait_expired(later));
        assert!(turn.job_wait_expired(now + limit));

        turn.background_jobs.clear();
        assert_eq!(turn.update_job_wait(later, limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_deadline, None);
    }

    /// The limit is a budget for the whole turn: each wait spends from it.
    #[test]
    fn job_waits_spend_one_budget_for_the_whole_turn() {
        let limit = StdDuration::from_secs(10);
        let mut turn = waiting_turn();
        let t0 = tokio::time::Instant::now();
        assert!(matches!(
            turn.update_job_wait(t0, limit),
            JobWaitChange::Entered { .. }
        ));
        // The jobs finish after 4 s.
        turn.background_jobs.clear();
        let t4 = t0 + StdDuration::from_secs(4);
        assert_eq!(turn.update_job_wait(t4, limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_spent, StdDuration::from_secs(4));

        // A new wait gets what is left, not a fresh limit.
        turn.background_jobs = vec![a_job()];
        let t5 = t0 + StdDuration::from_secs(5);
        assert_eq!(
            turn.update_job_wait(t5, limit),
            JobWaitChange::Entered {
                remaining: StdDuration::from_secs(6)
            }
        );
        assert_eq!(turn.job_wait_deadline, Some(t5 + StdDuration::from_secs(6)));
        // It lasts 6 s: the budget is gone.
        turn.background_jobs.clear();
        let t11 = t5 + StdDuration::from_secs(6);
        turn.update_job_wait(t11, limit);
        assert_eq!(turn.job_wait_spent, limit);

        // Entering with nothing left is refused, and `spent` is not reset.
        turn.background_jobs = vec![a_job()];
        assert_eq!(
            turn.update_job_wait(t11, limit),
            JobWaitChange::ExhaustedOnEntry
        );
        assert_eq!(turn.job_wait_deadline, None);
        assert_eq!(turn.job_wait_spent, limit);
    }

    #[test]
    fn the_job_wait_limit_reads_as_minutes_when_whole() {
        assert_eq!(describe_limit(StdDuration::from_secs(3600)), "60 min");
        assert_eq!(describe_limit(StdDuration::from_millis(300)), "300ms");
    }

    /// The issue's first case: a report-less turn whose job outlasts every
    /// nudge is never nudged, and reports once the CLI wakes it.
    #[tokio::test]
    async fn a_turn_waiting_on_its_job_is_not_nudged() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "raw", "line": one_job()},
                {"op": "text", "text": "started the gate"},
                {"op": "result"},
                {"op": "sleep", "seconds": 1.0},
                {"op": "raw", "line": no_jobs()},
                {"op": "init"},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        assert_eq!(
            session_notes(&pool, &session_id).await,
            vec!["background_jobs", "job_wait", "background_jobs"]
        );
    }

    /// The issue's third case: a job that never ends is bounded by the
    /// job-wait limit. The job runs in a session of its own, as Claude Code's
    /// Bash tool runs it, so the group kill does not reach it: the sweep that
    /// follows the kill does.
    #[tokio::test]
    async fn a_hung_job_closes_the_turn_as_no_report_at_the_limit() {
        let dir = TempDir::new();
        let child_pid_path = dir.0.join("child.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_escaped", "setsid": true, "pid_file": child_pid_path},
                {"op": "raw", "line": one_job()},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let timers = TurnTimers {
            job_wait_limit: StdDuration::from_millis(300),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let child_pid = read_pid_when_written(&child_pid_path).await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        assert_eq!(
            session_notes(&pool, &session_id).await,
            vec![
                "background_jobs",
                "job_wait",
                "no_report",
                "lingered",
                "leftovers_killed"
            ]
        );
        let notes = note_events(&pool, &session_id).await;
        let message = notes[2].payload["message"].as_str().unwrap();
        assert!(message.contains("hung gate"), "{message}");
        assert!(message.contains("background job"), "{message}");
        wait_until_gone(child_pid).await;
    }

    /// Once the list empties, the nudge clock starts from that moment.
    #[tokio::test]
    async fn nudging_resumes_after_the_jobs_are_gone() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "raw", "line": one_job()},
                {"op": "text", "text": "hmm"},
                {"op": "result"},
                {"op": "sleep", "seconds": 7.0},
                {"op": "raw", "line": no_jobs()},
                {"op": "answer_every_turn", "text": "still thinking"},
            ]),
        );
        let timers = TurnTimers {
            nudge_after: crate::test_support::RESPONSE_MARGIN,
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(2)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        assert_eq!(
            session_notes(&pool, &session_id).await,
            vec![
                "background_jobs",
                "job_wait",
                "background_jobs",
                "nudge",
                "nudge",
                "no_report"
            ]
        );
    }

    /// Output during the wait (a sub-agent's text, a changed list) never
    /// moves the deadline.
    #[tokio::test]
    async fn events_during_the_wait_do_not_push_the_deadline_back() {
        let dir = TempDir::new();
        let two_jobs = json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [
            {"task_id": "job1", "task_type": "local_bash", "description": "A"},
            {"task_id": "job2", "task_type": "local_bash", "description": "B"},
        ]});
        let mut steps = vec![
            json!({"op": "read_turn"}),
            json!({"op": "raw", "line": one_job()}),
            json!({"op": "result"}),
        ];
        for i in 0..15 {
            steps.push(json!({"op": "sleep", "seconds": 0.2}));
            if i % 2 == 0 {
                steps.push(json!({"op": "text", "text": "helper", "parent": "toolu_agent"}));
            } else {
                steps.push(json!({"op": "raw", "line": two_jobs}));
            }
        }
        steps.push(json!({"op": "sleep", "seconds": 60}));
        let binary = script_binary(&dir.0, serde_json::Value::Array(steps));
        let timers = TurnTimers {
            job_wait_limit: StdDuration::from_secs(1),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let notes = note_events(&pool, &session_id).await;
        let kinds: Vec<_> = notes
            .iter()
            .map(|e| e.payload["kind"].as_str().unwrap())
            .collect();
        assert!(!kinds.contains(&"nudge"), "{kinds:?}");
        let started = notes
            .iter()
            .find(|e| e.payload["kind"] == "job_wait")
            .unwrap();
        let closed = notes
            .iter()
            .find(|e| e.payload["kind"] == "no_report")
            .unwrap();
        let waited = closed.created_at - started.created_at;
        assert!(
            waited >= chrono::Duration::milliseconds(900)
                && waited < chrono::Duration::milliseconds(2500),
            "waited {waited}"
        );
    }

    /// A flood of output with no gaps can't starve the deadline: the
    /// `biased` select always has an event ready, so the expiry is also
    /// checked after each event.
    #[tokio::test]
    async fn continuous_output_does_not_starve_the_job_wait_deadline() {
        let dir = TempDir::new();
        let mut steps = vec![
            json!({"op": "read_turn"}),
            json!({"op": "raw", "line": one_job()}),
            json!({"op": "result"}),
        ];
        for _ in 0..20000 {
            steps.push(json!({"op": "text", "text": "x", "parent": "toolu_agent"}));
        }
        steps.push(json!({"op": "sleep", "seconds": 60}));
        let binary = script_binary(&dir.0, serde_json::Value::Array(steps));
        let timers = TurnTimers {
            job_wait_limit: StdDuration::from_millis(100),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        // The no_report note must land before the flood is over: events
        // stored after it prove the close didn't wait for the flood's end.
        let all = events::list_for_session(&pool, &session_id).await.unwrap();
        let at = all
            .iter()
            .position(|e| e.payload["kind"] == "no_report")
            .unwrap();
        let later = all.len() - at - 1;
        assert!(
            later > 100,
            "the close waited for the flood to end ({later})"
        );
    }

    async fn start_with_reaper_ready(
        binary: String,
        timers: TurnTimers,
    ) -> (SqlitePool, String, Arc<SessionManager>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
            timers,
        );
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();
        (pool, session_id, manager)
    }

    async fn reap_once(manager: &Arc<SessionManager>) {
        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;
    }

    #[tokio::test]
    async fn the_idle_reaper_leaves_a_turn_waiting_on_jobs_alone() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "raw", "line": one_job()},
                {"op": "result"},
                {"op": "sleep", "seconds": 1},
                {"op": "raw", "line": no_jobs()},
                {"op": "init"},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
            ]),
        );
        let (pool, session_id, manager) = start_with_reaper_ready(binary, fast_timers(3)).await;
        wait_for_note(&pool, &session_id, "job_wait").await;

        reap_once(&manager).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        let kinds = session_notes(&pool, &session_id).await;
        for unwanted in ["lingered", "no_report", "nudge"] {
            assert!(!kinds.iter().any(|k| k == unwanted), "{kinds:?}");
        }
    }

    /// Guards today's behaviour: with no job, the reaper still closes a
    /// stalled turn.
    #[tokio::test]
    async fn the_idle_reaper_still_closes_a_stalled_turn_with_no_job() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "hmm"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, session_id, manager) = start_with_reaper_ready(binary, timers).await;
        wait_until_events_len(&pool, &session_id, 3).await;

        reap_once(&manager).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::Reaped));
    }

    /// A sub-agent calling `report_outcome` does not complete the main
    /// agent's turn: the stage belongs to the main agent.
    #[tokio::test]
    async fn a_sub_agents_report_does_not_complete_the_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done", "parent": "toolu_agent"},
                {"op": "text", "text": "helper finished", "parent": "toolu_agent"},
                {"op": "result"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(0)).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));

        // And the sub-agent's rows are marked as such on the timeline.
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let sub_agent_rows = stored
            .iter()
            .filter(|e| e.payload["parent_tool_use_id"] == "toolu_agent")
            .count();
        assert_eq!(
            sub_agent_rows, 3,
            "tool call, tool result and text: {stored:?}"
        );
    }

    /// #92's case: the turn was working, the account hit its usage limit,
    /// and the CLI ended the turn. Not a crash, and not the agent's own
    /// failure — the run says so, which is what lets `retry` resume it.
    #[tokio::test]
    async fn a_usage_limit_ends_the_run_as_interrupted() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "editing files"},
                {"op": "usage_limit"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(
            binary,
            TurnTimers {
                grace: crate::test_support::LOAD_ALLOWANCE,
                ..fast_timers(0)
            },
        )
        .await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::Interrupted));

        // And the timeline says which rule recognised it, so a run
        // recognised only by the CLI's wording is visible as such.
        let detections: Vec<String> = events::list_for_session(&pool, &session_id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| {
                e.payload["detected_by"]
                    .as_str()
                    .map(|detected| detected.to_string())
            })
            .collect();
        assert_eq!(detections, vec!["structured", "message_text"]);
    }

    /// The same limit as seen by a daemon whose CLI puts nothing structured
    /// on the stream: the `result` line's text is all there is, and it is
    /// still recognised — labelled as the weaker evidence it is.
    #[tokio::test]
    async fn a_usage_limit_is_still_recognised_from_its_text_alone() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "usage_limit", "structured": false},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(
            binary,
            TurnTimers {
                grace: crate::test_support::LOAD_ALLOWANCE,
                ..fast_timers(0)
            },
        )
        .await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::Interrupted));

        let detections: Vec<String> = events::list_for_session(&pool, &session_id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| e.payload["detected_by"].as_str().map(str::to_string))
            .collect();
        assert_eq!(detections, vec!["message_text"]);
    }

    /// A limit that lands *after* the turn reported and ended is not an
    /// interruption: there is nothing left to resume, and the stage's
    /// outcome is already in.
    #[tokio::test]
    async fn a_limit_after_a_completed_turn_does_not_reopen_it() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "usage_limit"},
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(0)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
    }

    /// A report the tool rejected is not a report.
    #[tokio::test]
    async fn a_rejected_report_does_not_complete_the_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "lgtm", "is_error": true},
                {"op": "result"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(0)).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
    }

    /// A process that exits on its own without ever reporting didn't say it
    /// was done, however cleanly it exited.
    #[tokio::test]
    async fn a_clean_exit_without_a_report_is_not_a_completion() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "bye"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        // A silence window far longer than the test: the process exits on
        // its own long before any nudge could fire.
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        assert!(session_notes(&pool, &session_id).await.is_empty());
    }

    /// The other half of #88: a turn that reported and ended, but whose
    /// process (and what it started) kept running. It's killed once the
    /// grace period runs out, the run says so, and what it emitted after
    /// completing is flagged on the timeline instead of blending in.
    #[tokio::test]
    async fn a_process_that_outlives_its_completed_turn_is_killed_and_recorded_as_lingered() {
        let dir = TempDir::new();
        let heartbeat = dir.0.join("heartbeat");
        let child_pid_path = dir.0.join("child.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
                {"op": "spawn_child", "heartbeat": heartbeat, "pid_file": child_pid_path},
                {"op": "emit_forever", "text": "still going"},
            ]),
        );
        // Grace must fire exactly once, and only after the fixture has
        // spawned its child and emitted its post-completion output. With
        // the 400 ms default a loaded machine can kill the group first
        // (#126), so give the fixture `RESPONSE_MARGIN` (the max-elapsed
        // rule from #98) to get there; it is the real bound on the fixture.
        let timers = TurnTimers {
            grace: crate::test_support::RESPONSE_MARGIN,
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let child_pid = read_pid_when_written(&child_pid_path).await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::Lingered));
        wait_until_gone(child_pid).await;

        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let late = stored
            .iter()
            .filter(|e| e.payload["after_completion"] == true)
            .collect::<Vec<_>>();
        assert!(!late.is_empty(), "output after completion must be flagged");
        assert!(
            late.iter().all(|e| e.payload["text"] == "still going"),
            "only post-completion output is flagged: {late:?}"
        );
        assert_eq!(session_notes(&pool, &session_id).await, vec!["lingered"]);
    }

    /// A turn that reports, ends, and whose process exits promptly is the
    /// ordinary case: no kill, no note, `idle`.
    #[tokio::test]
    async fn a_completed_turn_whose_process_exits_in_time_is_idle() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(
            binary,
            TurnTimers {
                // The grace timer must never fire here (#98): a process that
                // exits promptly still finishes the test immediately.
                grace: crate::test_support::LOAD_ALLOWANCE,
                ..fast_timers(3)
            },
        )
        .await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        assert!(session_notes(&pool, &session_id).await.is_empty());
    }

    #[tokio::test]
    async fn an_errored_result_ends_the_turn_as_exited() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "result", "is_error": true},
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, None);
    }

    /// `session_meta` carries what the turn actually ran with, including the
    /// isolation the daemon launched it under.
    #[tokio::test]
    async fn session_meta_records_the_sessions_isolation() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        let cfg = RoleConfig {
            isolation: crate::adapter::Isolation::Isolated {
                skills: vec!["run-tests".to_string()],
                memory: false,
            },
            ..role_config()
        };
        manager
            .start(&session_id, "claude", "hello", &cfg, SessionKind::Standing)
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &session_id, 1).await;
        assert_eq!(stored[0].event_type, EventType::SessionMeta);
        assert_eq!(
            stored[0].payload["isolation"],
            json!({ "inherit_operator_config": false, "skills": ["run-tests"], "memory": false })
        );
    }

    // ---- end-reason precedence and the reviewer's #90 cases ----

    /// One row of [`final_run_state_precedence`]'s table.
    struct Case {
        name: &'static str,
        kind: SessionKind,
        turn: SingleShotTurn,
        clean_exit: bool,
        reaped: bool,
        cancelled: bool,
        stopping: bool,
        expected: (SessionStatus, Option<SessionEndReason>),
    }

    fn turn_with(f: impl FnOnce(&mut SingleShotTurn)) -> SingleShotTurn {
        let mut turn = SingleShotTurn::default();
        f(&mut turn);
        turn
    }

    /// `final_run_state` is where every way a run can end is decided, so its
    /// precedence is pinned case by case rather than only through the live
    /// tests that happen to reach some of them.
    #[test]
    fn final_run_state_precedence() {
        use SessionEndReason::{Cancelled, DaemonStopped, Interrupted, Lingered, NoReport, Reaped};
        use SessionKind::{SingleShot, Standing};
        use SessionStatus::{Exited, Idle};

        let silent = SingleShotTurn::default;
        let completed = || turn_with(|t| t.completed = true);
        let lingered_after_completion = || {
            turn_with(|t| {
                t.completed = true;
                t.lingered = true;
            })
        };
        let gave_up = || turn_with(|t| t.gave_up = true);
        let gave_up_and_killed = || {
            turn_with(|t| {
                t.gave_up = true;
                t.lingered = true;
            })
        };
        let errored = || turn_with(|t| t.errored = true);
        let errored_and_killed = || {
            turn_with(|t| {
                t.errored = true;
                t.lingered = true;
            })
        };
        let killed = || turn_with(|t| t.lingered = true);
        let waiting = || turn_with(|t| t.waiting_for_report = true);
        let interrupted = || {
            turn_with(|t| {
                t.interrupted = true;
                t.errored = true;
            })
        };
        let interrupted_then_killed = || {
            turn_with(|t| {
                t.interrupted = true;
                t.errored = true;
                t.lingered = true;
            })
        };

        let cases = vec![
            Case {
                name: "completed, clean exit",
                kind: SingleShot,
                turn: completed(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Idle, None),
            },
            Case {
                name: "completed, non-zero exit",
                kind: SingleShot,
                turn: completed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Idle, None),
            },
            Case {
                name: "completed then killed",
                kind: SingleShot,
                turn: lingered_after_completion(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(Lingered)),
            },
            Case {
                name: "never reported, killed",
                kind: SingleShot,
                turn: gave_up_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "never reported, exited on its own",
                kind: SingleShot,
                turn: silent(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "crashed",
                kind: SingleShot,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, None),
            },
            Case {
                name: "errored result, clean exit",
                kind: SingleShot,
                turn: errored(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, None),
            },
            Case {
                name: "errored result, killed",
                kind: SingleShot,
                turn: errored_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(Lingered)),
            },
            Case {
                name: "reaped, clean exit",
                kind: SingleShot,
                turn: silent(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                stopping: false,
                expected: (Idle, Some(Reaped)),
            },
            Case {
                name: "reaped, then killed",
                kind: SingleShot,
                turn: killed(),
                clean_exit: false,
                reaped: true,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(Reaped)),
            },
            Case {
                name: "reaped after giving up",
                kind: SingleShot,
                turn: gave_up(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "cancelled while waiting",
                kind: SingleShot,
                turn: waiting(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats lingered",
                kind: SingleShot,
                turn: lingered_after_completion(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats no report",
                kind: SingleShot,
                turn: gave_up_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats reaped",
                kind: SingleShot,
                turn: silent(),
                clean_exit: false,
                reaped: true,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled after completing cleanly",
                kind: SingleShot,
                turn: completed(),
                clean_exit: true,
                reaped: false,
                cancelled: true,
                stopping: false,
                expected: (Idle, Some(Cancelled)),
            },
            Case {
                name: "interrupted by a usage limit",
                kind: SingleShot,
                turn: interrupted(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(Interrupted)),
            },
            Case {
                // The kill is the more specific thing that happened, and
                // the one a human has to look at: something the turn
                // started was still running when it was killed.
                name: "lingered beats interrupted",
                kind: SingleShot,
                turn: interrupted_then_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(Lingered)),
            },
            Case {
                // Saw a limit, kept going, then ended its turn cleanly with
                // nothing reported: #90's case, not an interruption.
                name: "recovered from a limit, then never reported",
                kind: SingleShot,
                turn: turn_with(|t| t.interrupted = true),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "cancelled beats interrupted",
                kind: SingleShot,
                turn: interrupted(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "standing, clean exit",
                kind: Standing,
                turn: silent(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Idle, None),
            },
            Case {
                name: "standing, reaped",
                kind: Standing,
                turn: silent(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                stopping: false,
                expected: (Idle, Some(Reaped)),
            },
            Case {
                name: "standing, crashed",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: false,
                expected: (Exited, None),
            },
            Case {
                name: "standing, cancelled",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: true,
                cancelled: true,
                stopping: false,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "stopping, incomplete single-shot turn",
                kind: SingleShot,
                turn: waiting(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: true,
                expected: (Exited, Some(DaemonStopped)),
            },
            Case {
                name: "cancelled beats stopping",
                kind: SingleShot,
                turn: waiting(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "stopping beats reaped and no-report",
                kind: SingleShot,
                turn: gave_up_and_killed(),
                clean_exit: false,
                reaped: true,
                cancelled: false,
                stopping: true,
                expected: (Exited, Some(DaemonStopped)),
            },
            Case {
                name: "stopping, but the turn had completed",
                kind: SingleShot,
                turn: completed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: true,
                expected: (Idle, None),
            },
            Case {
                name: "standing, stopping",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                stopping: true,
                expected: (Idle, Some(DaemonStopped)),
            },
            Case {
                name: "standing, cancelled beats stopping",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                stopping: true,
                expected: (Exited, Some(Cancelled)),
            },
        ];
        for case in cases {
            assert_eq!(
                final_run_state(
                    case.kind,
                    &case.turn,
                    case.clean_exit,
                    case.reaped,
                    case.cancelled,
                    case.stopping
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    /// Reviewer finding on #90: a single-shot turn gone silent mid-turn (a
    /// hung tool call) and closed by the idle reaper, whose process then
    /// outlives the grace period, was recorded as `Lingered` — "kept running
    /// after its turn ended" — for a turn that never ended. It's `Reaped`.
    #[tokio::test]
    async fn a_reaper_closed_single_shot_turn_that_has_to_be_killed_is_recorded_as_reaped() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        // Zero idle timeout: the first reaper pass closes the turn.
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
            fast_timers(3),
        );
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();
        wait_until_events_len(&pool, &session_id, 1).await;

        manager
            .run_idle_reaper_loop(
                &IdleReaperConfig {
                    interval: StdDuration::from_millis(1),
                },
                Some(1),
            )
            .await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::Reaped));
        let notes = events::list_for_session(&pool, &session_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionNote)
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].payload["message"]
                .as_str()
                .is_some_and(|m| m.contains("idle reaper")),
            "{notes:?}"
        );
    }

    /// #88's surviving process was a single-shot coder run: cancelling one
    /// that is waiting for a report must kill it and say it was cancelled.
    #[tokio::test]
    async fn cancel_kills_a_single_shot_turn_waiting_for_its_report() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "waiting on a background agent"},
                {"op": "result"},
                {"op": "answer_every_turn", "text": "still waiting"},
            ]),
        );
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, session_id, manager) = start_single_shot(binary, timers).await;
        wait_until_events_len(&pool, &session_id, 3).await;

        manager.cancel(&session_id).await.unwrap();

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled));
    }

    /// Cancelling during the post-completion grace period: the operator's
    /// cancel wins over the lingering that would otherwise be recorded, and
    /// what the turn left running dies with it.
    #[tokio::test]
    async fn cancel_during_the_grace_period_kills_the_group_and_records_cancelled() {
        let dir = TempDir::new();
        let heartbeat = dir.0.join("heartbeat");
        let child_pid_path = dir.0.join("child.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "spawn_child", "heartbeat": heartbeat, "pid_file": child_pid_path},
                {"op": "emit_forever", "text": "still going"},
            ]),
        );
        let timers = TurnTimers {
            grace: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, session_id, manager) = start_single_shot(binary, timers).await;
        let child_pid = read_pid_when_written(&child_pid_path).await;

        manager.cancel(&session_id).await.unwrap();

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled));
        wait_until_gone(child_pid).await;
    }

    /// Reviewer finding on #90: a turn woken after a report-less `result`
    /// (here by nothing the daemon sent — the CLI starting a follow-up turn
    /// on its own, as it does for a background job's notification) is
    /// working again, and a silent stretch inside that turn must not be
    /// nudged. Also covers the CLI's repeated `init` line.
    #[tokio::test]
    async fn a_woken_turn_is_not_nudged_while_it_works() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "text", "text": "started a background job"},
                {"op": "result"},
                {"op": "init"},
                {"op": "text", "text": "the job finished; checking it"},
                {"op": "sleep", "seconds": 3.0},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
            ]),
        );
        // A pause comfortably longer than the nudge window, deliberately: a
        // turn wrongly treated as waiting would be nudged twice here, so the
        // empty note list below is the assertion that matters.
        // The 3 s sleep above stays longer than `nudge_after` (2 s), or the
        // test proves nothing. Remaining timing dependency (#98): the
        // result->init gap stays under 2 s.
        let timers = TurnTimers {
            nudge_after: crate::test_support::RESPONSE_MARGIN,
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert!(session_notes(&pool, &session_id).await.is_empty());

        let metas = events::list_for_session(&pool, &session_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionMeta)
            .collect::<Vec<_>>();
        assert_eq!(metas.len(), 2);
        assert_eq!(
            metas[0].payload["adapter_session_id"],
            metas[1].payload["adapter_session_id"]
        );
        assert_eq!(
            run.adapter_session_id.as_deref(),
            metas[0].payload["adapter_session_id"].as_str()
        );
    }

    /// Reviewer finding on #90: a completed turn whose CLI exits in time can
    /// still leave something running in its group that doesn't hold the
    /// daemon's pipes (a `nohup`ed server). It must not outlive the turn,
    /// since once the pid is retired nothing, not even cancel, can reach it.
    #[tokio::test]
    async fn a_completed_turns_detached_leftovers_are_killed_when_it_exits() {
        let dir = TempDir::new();
        let heartbeat = dir.0.join("heartbeat");
        let child_pid_path = dir.0.join("child.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_child", "heartbeat": heartbeat, "pid_file": child_pid_path, "detach": true},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
            ]),
        );
        let timers = TurnTimers {
            grace: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;
        let child_pid = read_pid_when_written(&child_pid_path).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        wait_until_gone(child_pid).await;
    }

    fn recording(name: &'static str) -> Arc<crate::recording_adapter::RecordingAdapter> {
        crate::recording_adapter::RecordingAdapter::new(name, &fixture_binary("fake_claude.py"))
    }

    #[tokio::test]
    async fn start_with_an_unknown_cli_reserves_and_spawns_nothing() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let claude = recording("claude");
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(claude.clone()),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        let err = manager
            .start(
                &session_id,
                "ghost",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::UnknownCli(_)), "{err:?}");
        let err = manager
            .resume(
                &session_id,
                "ghost",
                "S1",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::UnknownCli(_)), "{err:?}");
        assert!(claude.calls().is_empty());
        // Nothing was reserved: the same id can still start on a real name.
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &role_config(),
                SessionKind::Standing,
            )
            .await
            .unwrap();
    }

    async fn idle_chat_session_on(pool: &SqlitePool, cli: &str) -> String {
        let session_id = seed_session(pool).await;
        sqlx::query("UPDATE sessions SET cli_adapter = ? WHERE id = ?")
            .bind(cli)
            .bind(&session_id)
            .execute(pool)
            .await
            .unwrap();
        sessions::set_adapter_session_id(pool, &session_id, "S9")
            .await
            .unwrap();
        sessions::update_status(pool, &session_id, SessionStatus::Idle, None, None)
            .await
            .unwrap();
        session_id
    }

    #[tokio::test]
    async fn chat_resume_dispatches_to_the_sessions_recorded_adapter() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = idle_chat_session_on(&pool, "fake").await;
        let claude = recording("claude");
        let fake = recording("fake");
        let manager = SessionManager::new(
            pool.clone(),
            Registry::new(vec![claude.clone(), fake.clone()]),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        manager
            .send_message(&session_id, "hi", &role_config())
            .await
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![crate::recording_adapter::RecordedCall::Resume {
                adapter_session_id: "S9".to_string()
            }]
        );
        assert!(claude.calls().is_empty());
    }

    #[tokio::test]
    async fn chat_resume_of_a_session_on_an_unknown_adapter_is_refused_unchanged() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = idle_chat_session_on(&pool, "ghost").await;
        let claude = recording("claude");
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(claude.clone()),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        let err = manager
            .send_message(&session_id, "hi", &role_config())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::UnknownCli(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "role 'chat' uses cli 'ghost', which this daemon doesn't know; known CLIs: claude"
        );
        let row = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Idle);
        assert!(claude.calls().is_empty());
    }

    fn memory_role_config() -> RoleConfig {
        RoleConfig {
            isolation: crate::adapter::Isolation::Isolated {
                skills: Vec::new(),
                memory: true,
            },
            ..role_config()
        }
    }

    #[tokio::test]
    async fn chat_resume_on_omp_with_memory_is_refused_and_a_claude_one_is_not() {
        let dir = std::env::temp_dir().join(format!("choco-omp-chat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("omp-was-run");
        let wrapper = dir.join("omp-wrapper");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let pool = connect_in_memory().await.unwrap();
        let claude = recording("claude");
        let omp = Arc::new(crate::adapter::OmpAdapter::with_binary(
            wrapper.to_string_lossy(),
            dir.join("state"),
        ));
        let manager = SessionManager::new(
            pool.clone(),
            Registry::new(vec![claude.clone(), omp]),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        let session_id = idle_chat_session_on(&pool, "omp").await;
        let err = manager
            .send_message(&session_id, "hi", &memory_role_config())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::RoleRejected(_)), "{err:?}");
        assert!(err.to_string().contains("can't use memory: true"), "{err}");
        assert!(!marker.exists(), "omp must not have been started");
        assert!(
            !dir.join("state").exists(),
            "no overlay or session dir was made"
        );
        let row = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Idle);

        // The same role on claude is fine.
        let claude_session = idle_chat_session_on(&pool, "claude").await;
        manager
            .send_message(&claude_session, "hi", &memory_role_config())
            .await
            .unwrap();
        assert_eq!(claude.calls().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What one `turn_usage` row holds, as the scripted fixtures report it.
    struct ExpectedUsage {
        billing: &'static str,
        counting: &'static str,
        model: &'static str,
        /// `Some` when the fixture scripts the duration; omp measures its
        /// own wall time, so there it only has to be present.
        duration_ms: Option<i64>,
    }

    /// One assertion for every adapter: the `turn_usage` row a scripted
    /// turn leaves holds exactly the figures both fixtures script (300 /
    /// 60 / 90 / 15 tokens, 0.0369 USD, three model turns, one model).
    async fn assert_usage_row(pool: &SqlitePool, task_id: &str, expected: &ExpectedUsage) {
        type Row = (
            String,
            String,
            Option<f64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT billing, counting, cost_usd, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, duration_ms, model_turns, models
             FROM turn_usage WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (billing, counting, cost, input, output, read, write, duration, turns, models) =
            rows.into_iter().next().unwrap();
        assert_eq!(billing, expected.billing);
        assert_eq!(counting, expected.counting);
        assert!((cost.unwrap() - 0.0369).abs() < 1e-9, "{cost:?}");
        assert_eq!(
            (input, output, read, write),
            (Some(300), Some(60), Some(90), Some(15))
        );
        assert_eq!(turns, Some(3));
        match expected.duration_ms {
            Some(ms) => assert_eq!(duration, Some(ms)),
            None => assert!(duration.is_some_and(|ms| ms >= 0), "{duration:?}"),
        }
        let models: Value = serde_json::from_str(&models.unwrap()).unwrap();
        let models = models.as_object().unwrap();
        assert_eq!(models.len(), 1, "{models:?}");
        let figures = &models[expected.model];
        assert_eq!(figures["input_tokens"], 300);
        assert_eq!(figures["output_tokens"], 60);
        assert_eq!(figures["cache_read_tokens"], 90);
        assert_eq!(figures["cache_write_tokens"], 15);
        assert!((figures["cost_usd"].as_f64().unwrap() - 0.0369).abs() < 1e-9);
    }

    async fn run_scripted_turn(adapter: Arc<dyn AgentAdapter>, name: &str) -> (SqlitePool, String) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        sqlx::query("UPDATE sessions SET cli_adapter = ? WHERE id = ?")
            .bind(name)
            .bind(&session_id)
            .execute(&pool)
            .await
            .unwrap();
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            ..role_config()
        };
        manager
            .start(&session_id, name, "go", &cfg, SessionKind::Standing)
            .await
            .unwrap();
        let task_id = task_of(&pool, &session_id).await;
        crate::test_support::wait_until("a usage row", || async {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_usage WHERE task_id = ?")
                .bind(&task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if n >= 1 {
                Ok(())
            } else {
                Err("no row".to_string())
            }
        })
        .await;
        (pool, task_id)
    }

    #[tokio::test]
    async fn a_claude_turn_stores_the_scripted_usage() {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(fixture_binary(
            "fake_claude_usage.py",
        )));
        let (pool, task_id) = run_scripted_turn(adapter, "claude").await;
        assert_usage_row(
            &pool,
            &task_id,
            &ExpectedUsage {
                billing: "subscription",
                counting: "cumulative",
                model: "scripted-model",
                duration_ms: Some(1234),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn an_omp_turn_stores_the_same_scripted_usage() {
        let dir = std::env::temp_dir().join(format!("choco-omp-usage-{}", uuid::Uuid::new_v4()));
        let adapter: Arc<dyn AgentAdapter> = Arc::new(crate::adapter::OmpAdapter::with_binary(
            fixture_binary("fake_omp.py"),
            &dir,
        ));
        let (pool, task_id) = run_scripted_turn(adapter, "omp").await;
        assert_usage_row(
            &pool,
            &task_id,
            &ExpectedUsage {
                billing: "subscription",
                counting: "per_turn",
                model: "openai-codex/gpt-5.6-terra",
                duration_ms: None,
            },
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- leftovers: what a turn started is killed when it ends ------------

    /// An escaped job: its own session, started through an intermediate that
    /// exits, so it is neither in the agent's group nor its descendant.
    fn escaped_job(pid_file: &std::path::Path) -> serde_json::Value {
        json!({
            "op": "spawn_escaped", "setsid": true, "double_fork": true, "pid_file": pid_file,
        })
    }

    async fn start_leftovers(
        binary: String,
        timers: TurnTimers,
        idle: chrono::Duration,
        reader: Option<crate::proc_table::ProcReader>,
    ) -> (SqlitePool, String, Arc<SessionManager>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let registry = Registry::single(adapter);
        let notify = Arc::new(Notify::new());
        let manager = match reader {
            Some(reader) => SessionManager::with_proc_reader(
                pool.clone(),
                registry,
                idle,
                notify,
                timers,
                reader,
            ),
            None => SessionManager::with_turn_timers(pool.clone(), registry, idle, notify, timers),
        };
        manager
            .start(
                &session_id,
                "claude",
                "go",
                &single_shot_role_config(),
                SessionKind::SingleShot,
            )
            .await
            .unwrap();
        (pool, session_id, manager)
    }

    async fn note_message(pool: &SqlitePool, session_id: &str, kind: &str) -> Option<String> {
        note_events(pool, session_id)
            .await
            .into_iter()
            .find(|e| e.payload["kind"] == kind)
            .map(|e| {
                e.payload["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
    }

    async fn wait_for_turn_completed(pool: &SqlitePool, session_id: &str) {
        crate::test_support::wait_until("a turn_completed event", || async {
            let events = events::list_for_session(pool, session_id).await.unwrap();
            if events
                .iter()
                .any(|e| e.event_type == EventType::TurnCompleted)
            {
                Ok(())
            } else {
                Err("not yet".to_string())
            }
        })
        .await
    }

    enum EndAction {
        Nothing,
        Reap,
        Cancel,
        Shutdown,
    }

    /// Runs a fixture with one escaped job to the end of its turn and checks
    /// the job is gone and named in the `leftovers_killed` note.
    async fn assert_escaped_job_is_killed(
        extra_steps: Vec<serde_json::Value>,
        timers: TurnTimers,
        action: EndAction,
    ) -> (SqlitePool, String) {
        let dir = TempDir::new();
        let job_pid_file = dir.0.join("job.pid");
        let mut steps = vec![json!({"op": "read_turn"}), escaped_job(&job_pid_file)];
        steps.extend(extra_steps);
        let idle = match action {
            EndAction::Reap => chrono::Duration::zero(),
            _ => chrono::Duration::hours(1),
        };
        let binary = script_binary(&dir.0, json!(steps));
        let (pool, session_id, manager) = start_leftovers(binary, timers, idle, None).await;
        let job = read_pid_when_written(&job_pid_file).await;
        match action {
            EndAction::Nothing => {}
            EndAction::Reap => {
                wait_for_turn_completed(&pool, &session_id).await;
                reap_once(&manager).await;
            }
            EndAction::Cancel => manager.cancel(&session_id).await.unwrap(),
            EndAction::Shutdown => manager.shutdown(StdDuration::from_secs(20)).await,
        }
        wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        let note = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(note.contains(&format!("{job} (")), "{note}");
        (pool, session_id)
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_cli_exits_on_its_own_after_reporting() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![
                json!({"op": "report", "outcome": "done"}),
                json!({"op": "result"}),
                json!({"op": "exit"}),
            ],
            fast_timers(3),
            EndAction::Nothing,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        // The sweep does not change how the turn ended.
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_cli_lingers_after_reporting() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![
                json!({"op": "report", "outcome": "done"}),
                json!({"op": "result"}),
                json!({"op": "sleep", "seconds": 60}),
            ],
            fast_timers(3),
            EndAction::Nothing,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::Lingered));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_turn_closes_as_no_report_after_nudges() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![
                json!({"op": "result"}),
                json!({"op": "sleep", "seconds": 60}),
            ],
            fast_timers(1),
            EndAction::Nothing,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_job_wait_limit_closes_the_turn() {
        let timers = TurnTimers {
            job_wait_limit: StdDuration::from_millis(300),
            ..fast_timers(3)
        };
        let (pool, id) = assert_escaped_job_is_killed(
            vec![
                json!({"op": "raw", "line": one_job()}),
                json!({"op": "result"}),
                json!({"op": "sleep", "seconds": 60}),
            ],
            timers,
            EndAction::Nothing,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_turn_ends_with_an_error_result() {
        assert_escaped_job_is_killed(
            vec![
                json!({"op": "result", "is_error": true}),
                json!({"op": "sleep", "seconds": 60}),
            ],
            fast_timers(3),
            EndAction::Nothing,
        )
        .await;
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_idle_reaper_closes_the_turn() {
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            ..fast_timers(3)
        };
        let (pool, id) = assert_escaped_job_is_killed(
            vec![
                json!({"op": "result"}),
                json!({"op": "sleep", "seconds": 60}),
            ],
            timers,
            EndAction::Reap,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::Reaped));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_on_cancel() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![json!({"op": "sleep", "seconds": 60})],
            fast_timers(3),
            EndAction::Cancel,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_on_shutdown() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![json!({"op": "sleep", "seconds": 60})],
            fast_timers(3),
            EndAction::Shutdown,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.end_reason, Some(SessionEndReason::DaemonStopped));
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_cli_crashes_mid_turn() {
        let (pool, id) = assert_escaped_job_is_killed(
            vec![json!({"op": "exit", "code": 3})],
            fast_timers(3),
            EndAction::Nothing,
        )
        .await;
        let run = sessions::get(&pool, &id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, None);
    }

    /// A job whose environment is scrubbed is found only through the pids
    /// recorded while it was still the agent's descendant (`BackgroundJobs`).
    #[tokio::test]
    async fn a_job_without_the_marker_is_found_through_the_pids_recorded_at_its_announcement() {
        let dir = TempDir::new();
        let job_pid_file = dir.0.join("job.pid");
        let release = dir.0.join("release");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_escaped", "setsid": true, "double_fork": true, "scrub_env": true,
                 "announce": true, "release_file": release, "pid_file": job_pid_file},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;
        let job = read_pid_when_written(&job_pid_file).await;
        // Recorded before the event is stored, so seeing the note means the
        // job (still a descendant) is on the list. Only then let its
        // intermediate parent go.
        wait_for_note(&pool, &session_id, "background_jobs").await;
        std::fs::write(&release, "").unwrap();
        wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        let note = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .unwrap();
        assert!(note.contains(&format!("{job} (")), "{note}");
    }

    /// A job nobody announced, with no readable marker, still the agent's
    /// descendant when the turn reports. The CLI then lets it be orphaned and
    /// exits on its own: only the pids recorded at `TurnCompleted` link the
    /// job to the turn.
    #[tokio::test]
    async fn an_unannounced_unmarked_job_is_found_through_the_pids_recorded_at_turn_completed() {
        let dir = TempDir::new();
        let job_pid_file = dir.0.join("job.pid");
        let release = dir.0.join("release");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_escaped", "setsid": true, "double_fork": true, "scrub_env": true,
                 "defer_release": true, "pid_file": job_pid_file},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "release_escaped", "release_file": release},
                {"op": "exit"},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;
        let job = read_pid_when_written(&job_pid_file).await;
        // The descendants are recorded before the event is stored.
        wait_for_turn_completed(&pool, &session_id).await;
        std::fs::write(&release, "").unwrap();
        wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        let note = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .unwrap();
        assert!(note.contains(&format!("{job} (")), "{note}");
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
    }

    /// An orphaned session nobody announced: one process carries the marker,
    /// one does not. The first is a seed; the second is owned through the
    /// session.
    #[tokio::test]
    async fn the_markerless_member_of_an_orphaned_session_is_killed_with_its_marked_one() {
        let dir = TempDir::new();
        let marked = dir.0.join("marked.pid");
        let unmarked = dir.0.join("unmarked.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_escaped", "setsid": true, "double_fork": true,
                 "pid_file": marked, "child_pid_file": unmarked},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;
        let marked = read_pid_when_written(&marked).await;
        let unmarked = read_pid_when_written(&unmarked).await;
        wait_until_final(&pool, &session_id).await;
        wait_until_gone(marked).await;
        wait_until_gone(unmarked).await;
    }

    #[tokio::test]
    async fn a_job_that_ignores_sigterm_is_killed() {
        let dir = TempDir::new();
        let job_pid_file = dir.0.join("job.pid");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_escaped", "setsid": true, "double_fork": true,
                 "ignore_sigterm": true, "pid_file": job_pid_file},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;
        let job = read_pid_when_written(&job_pid_file).await;
        wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
    }

    /// Three processes the turn did not start survive a sweep.
    struct Bystanders(Vec<std::process::Child>);

    impl Bystanders {
        fn start() -> Self {
            use std::os::unix::process::CommandExt;
            let in_own_session = |mut command: std::process::Command| {
                // SAFETY: `setsid` is async-signal-safe.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                command.spawn().unwrap()
            };
            let own_session = in_own_session({
                let mut c = std::process::Command::new("sleep");
                c.arg("600");
                c
            });
            let other_marker = in_own_session({
                let mut c = std::process::Command::new("python3");
                c.args(["-c", "import time; time.sleep(600)"])
                    .env("CHOCOFACTORY_TURN_00000000000000000000000000000001", "1");
                c
            });
            let plain_child = std::process::Command::new("sleep")
                .arg("600")
                .spawn()
                .unwrap();
            Bystanders(vec![own_session, other_marker, plain_child])
        }

        fn all_alive(&self) -> bool {
            self.0.iter().all(|c| process_alive(c.id()))
        }
    }

    impl Drop for Bystanders {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[tokio::test]
    async fn a_sweep_never_kills_what_the_turn_did_not_start() {
        for action in [EndAction::Cancel, EndAction::Nothing] {
            let bystanders = Bystanders::start();
            let extra = match action {
                EndAction::Cancel => vec![json!({"op": "sleep", "seconds": 60})],
                _ => vec![
                    json!({"op": "report", "outcome": "done"}),
                    json!({"op": "result"}),
                    json!({"op": "sleep", "seconds": 60}),
                ],
            };
            assert_escaped_job_is_killed(extra, fast_timers(3), action).await;
            assert!(bystanders.all_alive(), "a sweep killed a bystander");
        }
    }

    /// With the table unreadable, the group kill is still sent, the turn ends
    /// the way it does with a working reader, and the operator is told.
    #[tokio::test]
    async fn an_unreadable_process_table_is_reported_and_changes_nothing_else() {
        async fn run(
            reader: Option<crate::proc_table::ProcReader>,
        ) -> (SessionStatus, Option<SessionEndReason>, Vec<String>) {
            let dir = TempDir::new();
            let agent_pid_file = dir.0.join("agent.pid");
            let binary = script_binary(
                &dir.0,
                json!([
                    {"op": "read_turn"},
                    {"op": "run", "command": format!("echo $PPID > {}", agent_pid_file.display())},
                    {"op": "report", "outcome": "done"},
                    {"op": "result"},
                    {"op": "sleep", "seconds": 60},
                ]),
            );
            let (pool, session_id, _manager) =
                start_leftovers(binary, fast_timers(3), chrono::Duration::hours(1), reader).await;
            let agent = read_pid_when_written(&agent_pid_file).await;
            let run = wait_until_final(&pool, &session_id).await;
            wait_until_gone(agent).await;
            (
                run.status,
                run.end_reason,
                session_notes(&pool, &session_id).await,
            )
        }
        let broken: crate::proc_table::ProcReader =
            Arc::new(|_| Err(std::io::Error::other("injected failure")));
        let (status, reason, notes) = run(Some(broken)).await;
        let (good_status, good_reason, good_notes) = run(None).await;
        assert_eq!((status, reason), (good_status, good_reason));
        assert_eq!(reason, Some(SessionEndReason::Lingered));
        assert!(
            notes.contains(&"leftovers_unchecked".to_string()),
            "{notes:?}"
        );
        assert!(!good_notes.contains(&"leftovers_unchecked".to_string()));
    }

    /// Entering a wait with the per-turn budget already spent closes the turn
    /// at once: no deadline, no `job_wait` note, a message that says why.
    #[tokio::test]
    async fn a_wait_entered_with_no_budget_left_closes_the_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "raw", "line": one_job()},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            job_wait_limit: StdDuration::ZERO,
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let notes = session_notes(&pool, &session_id).await;
        assert!(!notes.contains(&"job_wait".to_string()), "{notes:?}");
        let message = note_message(&pool, &session_id, "no_report").await.unwrap();
        assert!(message.contains("already been used up"), "{message}");
    }

    /// A grace-site kill that finds nothing alive and sweeps nothing does not
    /// count as the agent lingering: no `lingered` note, not `Lingered`.
    #[tokio::test]
    async fn a_grace_kill_that_finds_nothing_alive_is_not_lingering() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let empty: crate::proc_table::ProcReader = Arc::new(|_| Ok(Vec::new()));
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(empty),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_ne!(run.end_reason, Some(SessionEndReason::Lingered));
        let notes = session_notes(&pool, &session_id).await;
        assert!(!notes.contains(&"lingered".to_string()), "{notes:?}");
    }

    /// A process the sweep cannot get rid of is listed as a survivor in the
    /// `leftovers_killed` note rather than dropped.
    #[tokio::test]
    async fn a_process_that_survives_the_sweep_is_listed_in_the_note() {
        const GHOST: i32 = 99_999_999;
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        // SAFETY: only reads the process's own uid.
        let uid = unsafe { libc::geteuid() };
        let reader: crate::proc_table::ProcReader = Arc::new(move |_| {
            Ok(vec![crate::proc_table::ProcEntry {
                pid: GHOST,
                ppid: 1,
                pgid: GHOST,
                sid: GHOST,
                uid,
                start: 1,
                zombie: false,
                comm: "ghost".into(),
                marker: crate::proc_table::MarkerStatus::Present,
            }])
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        wait_until_final(&pool, &session_id).await;
        let message = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(message.contains("could not be killed"), "{message}");
        assert!(message.contains(&format!("{GHOST} (ghost)")), "{message}");
        // Listed once, though several sites swept.
        assert_eq!(message.matches("(ghost)").count(), 1, "{message}");
    }

    /// A leftover the daemon is not permitted to kill (EPERM) is listed as a
    /// survivor, once, by the per-pid failure path.
    #[tokio::test]
    async fn a_leftover_that_cannot_be_killed_is_listed_as_a_survivor() {
        // SAFETY: only reads ids and probes pids with signal 0.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            eprintln!("skipped: running as root, every pid is signalable");
            return;
        }
        // A pid we may not signal: some other user's process.
        let foreign = (2..5000).find(|&pid| {
            // SAFETY: signal 0 only checks permission. The pid must name a
            // process *group* we may not signal, since the kill is a killpg.
            let rc = unsafe { libc::killpg(pid, 0) };
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        });
        let Some(foreign) = foreign else {
            eprintln!("skipped: no foreign pid found");
            return;
        };
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let reader: crate::proc_table::ProcReader = Arc::new(move |_| {
            Ok(vec![crate::proc_table::ProcEntry {
                pid: foreign,
                ppid: 1,
                pgid: foreign,
                sid: foreign,
                uid,
                start: 1,
                zombie: false,
                comm: "foreign".into(),
                marker: crate::proc_table::MarkerStatus::Present,
            }])
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        wait_until_final(&pool, &session_id).await;
        let message = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(message.starts_with("1 could not be killed"), "{message}");
        assert_eq!(message.matches("(foreign)").count(), 1, "{message}");
    }

    /// A live agent group the daemon may not signal (EPERM) is listed as a
    /// survivor by `kill_and_sweep`'s group-failure path.
    #[tokio::test]
    async fn a_group_that_cannot_be_killed_is_listed_as_a_survivor() {
        // SAFETY: only reads the uid and probes pids with signal 0.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            eprintln!("skipped: running as root, every pid is signalable");
            return;
        }
        let foreign = (2..5000).find(|&pid| {
            // SAFETY: signal 0 only checks permission. The pid must name a
            // process *group* we may not signal, since the kill is a killpg.
            let rc = unsafe { libc::killpg(pid, 0) };
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        });
        let Some(foreign) = foreign else {
            eprintln!("skipped: no foreign pid found");
            return;
        };
        let reader: crate::proc_table::ProcReader = Arc::new(move |_| {
            Ok(vec![crate::proc_table::ProcEntry {
                pid: foreign,
                ppid: 1,
                pgid: foreign,
                sid: foreign,
                uid,
                start: 1,
                zombie: false,
                comm: "foreign".into(),
                marker: crate::proc_table::MarkerStatus::Absent,
            }])
        });
        let tracker = LeftoverTracker::new(
            "CHOCOFACTORY_TURN_x".into(),
            SessionKind::SingleShot,
            reader,
        );
        let report = kill_and_sweep(&tracker, foreign as u32, "test").await;
        assert!(
            matches!(report.group, GroupKill::Failed(_)),
            "{:?}",
            describe_group_kill(&report.group)
        );
        let state = tracker.state();
        let listed = state
            .survivors
            .iter()
            .filter(|(p, _)| *p == foreign)
            .count();
        assert_eq!(listed, 1, "{:?}", state.survivors);
    }

    /// Short waits that each end well inside the limit still add up to it.
    #[tokio::test]
    async fn waits_on_short_jobs_add_up_to_one_per_turn_limit() {
        let dir = TempDir::new();
        let block = json!([
            {"op": "raw", "line": one_job()},
            {"op": "result"},
            {"op": "sleep", "seconds": 0.8},
            {"op": "raw", "line": no_jobs()},
            {"op": "init"},
        ]);
        let mut steps = vec![json!({"op": "read_turn"})];
        for _ in 0..4 {
            steps.extend(block.as_array().unwrap().clone());
        }
        let binary = script_binary(&dir.0, json!(steps));
        let timers = TurnTimers {
            nudge_after: StdDuration::from_secs(3600),
            job_wait_limit: StdDuration::from_secs(2),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let message = note_message(&pool, &session_id, "no_report").await.unwrap();
        assert!(message.contains("a total per turn"), "{message}");
    }

    #[test]
    fn lingered_is_decided_from_the_kill_outcomes() {
        let err = || std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(!lingered_decision(&GroupKill::NothingAlive, 0));
        assert!(lingered_decision(&GroupKill::Killed(1), 0));
        assert!(lingered_decision(&GroupKill::NothingAlive, 1));
        assert!(lingered_decision(&GroupKill::Failed(err()), 0));
        assert!(lingered_decision(
            &GroupKill::Unknown {
                scan_error: err(),
                signal_result: Ok(())
            },
            0
        ));
    }

    /// Pids of this user's processes whose program name ends with `program`
    /// and whose command line contains `needle`: the process itself, not a
    /// shell whose command line merely mentions it.
    fn pids_running(program: &str, needle: &str) -> Vec<u32> {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let pid = words.next()?.parse().ok()?;
                let name = words.next()?;
                let name_matches = name.rsplit('/').next()?.to_lowercase().starts_with(program);
                (name_matches && line.contains(needle)).then_some(pid)
            })
            .collect()
    }

    /// Opt-in probe against the real `claude` (a few cents on haiku): a
    /// turn that starts a background `sleep` and a foreground `nohup`ed
    /// python, ends without reporting, and is closed at the job-wait limit.
    /// Both must be gone afterwards.
    ///
    /// `CHOCOFACTORY_REAL_CLAUDE_TESTS=1 cargo test -p chocofactoryd
    /// the_real_claude_turns_leftovers_are_killed -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "drives the real claude; set CHOCOFACTORY_REAL_CLAUDE_TESTS=1"]
    async fn the_real_claude_turns_leftovers_are_killed() {
        assert_eq!(
            std::env::var("CHOCOFACTORY_REAL_CLAUDE_TESTS").as_deref(),
            Ok("1"),
            "set CHOCOFACTORY_REAL_CLAUDE_TESTS=1 to run this test against the real claude"
        );
        assert!(
            std::env::var_os("CHOCOFACTORY_CLAUDE_BINARY").is_none(),
            "CHOCOFACTORY_CLAUDE_BINARY is set; this probe must drive the real claude"
        );
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::new());
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
            TurnTimers {
                job_wait_limit: StdDuration::from_secs(20),
                ..fast_timers(3)
            },
        );
        let dir = TempDir::new();
        let cfg = RoleConfig {
            model: Some("haiku".to_string()),
            cwd: dir.0.clone(),
            // A disposable directory, and Bash calls need no approval there.
            sandboxed: true,
            ..single_shot_role_config()
        };
        let prompt = "Do exactly this, then stop. First, one Bash call with run_in_background \
             true running: /bin/sleep 600 . Second, a separate foreground Bash call running: \
             nohup python3 -c \"import time; time.sleep(601)\" >/dev/null 2>&1 & \
             After both calls, end your turn with the single word ok. Do not wait for the \
             commands and do not call report_outcome.";
        manager
            .start(&session_id, "claude", prompt, &cfg, SessionKind::SingleShot)
            .await
            .unwrap();

        // Recorded while the turn is still open.
        let (sleep_pid, python_pid) =
            crate::test_support::wait_until("both leftovers to be running", || async {
                let sleeps = pids_running("sleep", "sleep 600");
                let pythons = pids_running("python", "time.sleep(601)");
                match (sleeps.first(), pythons.first()) {
                    (Some(a), Some(b)) => Ok((*a, *b)),
                    _ => Err(format!("sleep {sleeps:?}, python {pythons:?}")),
                }
            })
            .await;
        println!("leftovers before the close: sleep {sleep_pid}, python {python_pid}");

        let run = wait_until_final(&pool, &session_id).await;
        println!("end_reason {:?}", run.end_reason);
        for note in note_events(&pool, &session_id).await {
            println!("note {}: {}", note.payload["kind"], note.payload["message"]);
        }
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        wait_until_gone(sleep_pid).await;
        wait_until_gone(python_pid).await;
    }
}
