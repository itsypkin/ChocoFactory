use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::proc_table::{self, Identity, OwnershipInput, ProcEntry, ProcReader};
use crate::shell::GroupKill;

use chocofactory_core::models::{EventType, SessionCleanup, SessionEndReason, SessionStatus};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::adapter::{
    AdapterError, AgentEvent, AgentHandle, BackgroundJob, Registry, RoleConfig, SettingsVerdict,
    UnknownCliError, describe_background_work,
};
use crate::db::{events, sessions, usage};

/// Why [`SessionManager::stop`] kills a session. The reason is recorded
/// before the kill, so the run ends with the matching end reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Cancelled,
    Paused,
}

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
    /// Set by `SessionManager::stop(.., StopReason::Paused)` before the kill,
    /// exactly as `cancelled` is: the reason travels through shared state and
    /// the kill happens inline. Ranks below `cancelled` and above `stopping`.
    paused: Arc<AtomicBool>,
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
            .validate_role(&session_row.role, &cfg.isolation, &cfg.claude_settings)
            .map_err(SessionError::RoleRejected)?;
        if let Some(model) = &cfg.model {
            adapter
                .validate_model(&session_row.role, model)
                .map_err(SessionError::RoleRejected)?;
        }

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
        // Standing: a chat resume tracks no cleanup, so this clears it too.
        if let Err(err) = sessions::update_status(
            &self.pool,
            session_id,
            SessionStatus::Active,
            None,
            None,
            None,
        )
        .await
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
        self.stop(session_id, StopReason::Cancelled).await
    }

    /// Kills a live session's process group, recording `reason` first so the
    /// run ends as `Cancelled` or `Paused` (a pause `--now`, #295). Rank:
    /// Cancelled > Paused > DaemonStopped. Everything `cancel` documents
    /// holds for both reasons.
    pub async fn stop(&self, session_id: &str, reason: StopReason) -> Result<(), SessionError> {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
            Some(SessionSlot::Live(session)) => {
                // Ordered before the kill, not after: killing the group
                // closes the subprocess's pipes, which can unwind
                // `drain_session` to its `end_reason` read on another task
                // immediately. Setting the flag afterwards would leave that
                // read racing this write and reporting a cancelled run as
                // an ordinary crash.
                match reason {
                    StopReason::Cancelled => {
                        session.signals.cancelled.store(true, Ordering::SeqCst)
                    }
                    StopReason::Paused => session.signals.paused.store(true, Ordering::SeqCst),
                }
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
            paused: Arc::new(AtomicBool::new(false)),
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
/// main agent (not a sub-agent, not one the tool rejected). (For omp, the
/// `result` is the `TurnCompleted` of one native execution: its closing
/// `agent_end` with `yielded: true`, never a prompt's result or a
/// settlement; a woken run has its own.)
///
/// 1. Report, then `result`: the turn is complete. Stdin is closed so the CLI
///    exits, and a grace timer starts. If the process is still alive when it
///    runs out, its whole group is killed. The run then ends `Lingered`, the
///    turn having said it was done while something it started kept going,
///    unless the cleanup was verified: with the group gone, the last check
///    empty and the child reaped, the turn is complete (`Idle`) as if it had
///    left nothing running.
/// 2. `result` with no report: the turn is waiting (or forgot). Stdin stays
///    open, so a background job's notification can wake it. After
///    `nudge_after` with no events at all it is nudged on stdin, up to
///    `max_nudges` times; after that, stdin is closed, the grace rule
///    applies, and the run ends `NoReport`. While the CLI signals background
///    work as pending (claude from its running jobs, omp from
///    `awaitingAsyncWork`), the turn is not nudged: it is closed the same way
///    once the waits add up to `job_wait_limit` in total, and the idle reaper
///    leaves it alone (#271). A start by the main agent ends a wait, running
///    time spends none of the budget, and a later wait gets only what is
///    left.
/// 3. A `result` with `is_error` closes stdin and applies the grace rule; the
///    run ends `exited`, as a crash would.
///
/// `idle` is written only once the process has actually gone, so the engine's
/// watcher can never advance a workflow while the turn's process is still
/// running. Anything the process emits after completion is still recorded,
/// flagged `after_completion`, rather than silently appended to a run the
/// engine may already be acting on.
///
/// The final status write also records `cleanup` for a single-shot turn:
/// verified only when the end-of-turn sweep ran, the output stream ended on
/// its own, the last check saw nothing alive with no failed process-table read
/// in the turn, and the child was reaped. Only verified cleanup changes how a
/// turn ended, and only in two cases where a grace kill would otherwise have
/// ended it `Lingered`: a completed turn stays complete, and a turn whose main
/// agent was cut off by a usage limit with nothing after it ends
/// `Interrupted`. Everything else about the end is decided without it.
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
        paused,
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
    // Set only when the output stream ended on its own. Stays false when the
    // kill-settle timer below gives up on a stream something outside the
    // group still holds open: that process is alive and was not reached.
    let mut stream_ended = false;
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
                let Some(event) = event else {
                    stream_ended = true;
                    break;
                };
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
                // Whether or not the prompt was written, the turn is over:
                // kill the process group now, the way `cancel` does, and
                // record why for the end reason.
                if let AgentEvent::SettingsNotApplied { verdict, .. } = &event {
                    turn.settings_end = Some(match verdict {
                        SettingsVerdict::Rejected => SessionEndReason::ClaudeSettingsRejected,
                        SettingsVerdict::Unconfirmed { .. } => {
                            SessionEndReason::ClaudeSettingsUnconfirmed
                        }
                    });
                    tracing::warn!(session_id, "claude settings not applied; killing the session");
                    if let Some(pgid) = *pgid.lock().await {
                        kill_and_sweep(&tracker, pgid, "settings not applied").await;
                    }
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
                                &turn,
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
                //
                // The kills an earlier site made in its extra rounds are
                // taken before this sweep: they count here even if those
                // processes have since exited, and are not counted again if
                // this sweep finds one still alive. Counted, not signalled.
                let mut carried = 0;
                let mut ordinary = 0;
                let report = match *pgid.lock().await {
                    Some(pgid) => {
                        carried = tracker.state().carry_late_kills();
                        let mut report = kill_and_sweep(&tracker, pgid, "grace").await;
                        ordinary = report.sweep_killed;
                        report.sweep_killed += carried;
                        Some(report)
                    }
                    None => None,
                };
                if let Some(report) = &report {
                    turn.lingered = lingered_after(report);
                }
                if turn.lingered
                    && let Some(report) = &report
                {
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
                        group = %describe_group_kill(&report.group),
                        earlier_kills_carried = carried,
                        "grace period over; tried to signal the agent's process group and sweep what \
                         the turn started; this does not say a process was seen running"
                    );
                    let grace_s = turn_timers.grace.as_secs_f32();
                    // Only the earlier cleanup's kill is counted here. Whether
                    // grace's own sweep still saw or signalled that process (or
                    // anything) is not known from the counts, so the note does
                    // not say.
                    let message = if carried > 0
                        && ordinary == 0
                        && matches!(report.group, GroupKill::NothingAlive)
                    {
                        lingered_carried_message(grace_s, after)
                    } else {
                        lingered_message(&report.group, grace_s, after)
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
    let mut swept = false;
    {
        let mut pgid = pgid.lock().await;
        if kind == SessionKind::SingleShot
            && let Some(group) = *pgid
        {
            // The report is dropped on purpose: the tracker holds everything
            // a note needs from it. A group kill that signals live members
            // here adds no note of its own. The leader is often still
            // exiting at this point, and a note for members that were only
            // signalled would bring back the false note (and the flake) that
            // the sweep's same-group exemption exists to prevent.
            kill_and_sweep(&tracker, group, "end of turn").await;
            swept = true;
        }
        *pgid = None;
    }
    let observed_clean = if kind == SessionKind::SingleShot {
        write_leftover_notes(pool, session_id, &tracker, events_notify).await
    } else {
        false
    };

    let exit_status = handle.wait().await;
    let clean_exit = matches!(&exit_status, Ok(status) if status.success());
    if let Err(err) = &exit_status {
        tracing::error!(session_id, %err, "failed to reap subprocess");
    }
    // Verified only when every part holds; anything else is unverified. An
    // `Ok` from `wait` proves the reap even for a non-zero exit code. A
    // standing session tracks no cleanup.
    let cleanup = match kind {
        SessionKind::Standing => None,
        SessionKind::SingleShot => Some(
            if swept && stream_ended && observed_clean && exit_status.is_ok() {
                SessionCleanup::Verified
            } else {
                SessionCleanup::Unverified
            },
        ),
    };

    let (final_status, end_reason) = final_run_state_ranked(
        kind,
        &turn,
        clean_exit,
        reaped,
        cancelled.load(Ordering::SeqCst),
        paused.load(Ordering::SeqCst),
        stopping.load(Ordering::SeqCst),
        cleanup,
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
    // `status`, `end_reason` and `cleanup` are set in the one statement below
    // rather than several: a watcher elsewhere (engine.rs's turn-completion
    // watcher) polls this row from a separate task and must never be able to
    // observe `status == Idle` while `end_reason` still holds a stale (or
    // absent) value from before this exit — that's exactly the gap that
    // would resurrect the ambiguity `end_reason` exists to close. The same
    // statement carries `cleanup`, so a refused write leaves the row
    // `Active` and `Pending` together instead of one without the other.
    if let Err(err) = sessions::update_status(
        pool,
        session_id,
        final_status,
        ended_at,
        end_reason,
        cleanup,
    )
    .await
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
    /// A clean end (claude's `result`, omp's yielded `agent_end`) followed a
    /// successful report: the turn is done.
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
    /// A usage limit was the last thing the main agent's open turn saw: set by
    /// `Interrupted`, cleared by any main activity or a clean end. Only
    /// meaningful until the turn's error end consumes it.
    cutoff_pending: bool,
    /// The turn's terminating error end arrived while `cutoff_pending` held:
    /// the main agent was working, hit the limit and was cut off with nothing
    /// after it. Unlike `interrupted`, a limit the agent recovered from, or one
    /// that arrived after the turn had closed, never sets it.
    cutoff_confirmed: bool,
    /// The claude adapter found the role's `claude_settings` not applied (or
    /// unconfirmed) (#346); holds the end reason to record.
    /// Set for a standing session too.
    settings_end: Option<SessionEndReason>,
    /// The background jobs the CLI last named as running (#271). Each
    /// `BackgroundJobs` event replaces it. Display only: omp names none, so an
    /// empty list is not evidence that nothing is pending.
    background_jobs: Vec<BackgroundJob>,
    /// The CLI says background work can still wake the agent. This, not the
    /// list above, decides whether the turn is waiting on jobs. Each
    /// `BackgroundJobs` event replaces it.
    background_pending: bool,
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
            cutoff_pending: false,
            cutoff_confirmed: false,
            settings_end: None,
            background_jobs: Vec::new(),
            background_pending: false,
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
        if let AgentEvent::BackgroundJobs { running, pending } = event {
            self.background_jobs = running.clone();
            self.background_pending = *pending;
            return TurnStep::Continue;
        }
        if self.completed || self.errored || self.gave_up {
            return TurnStep::Continue;
        }
        // Main events only: a sub-agent's arrive wrapped in
        // `AgentEvent::Subagent` and match nothing here. A session's metadata,
        // background-job levels, protocol errors and errors show no work, so
        // they leave the cutoff as it was.
        match event {
            AgentEvent::Interrupted { .. } => self.cutoff_pending = true,
            AgentEvent::AssistantMessage { .. }
            | AgentEvent::Thinking { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::ExecutionStarted => self.cutoff_pending = false,
            _ => {}
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
                    // The CLI's own start of a main execution is activity
                    // even before it produces any output.
                    | AgentEvent::ExecutionStarted
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
            // A diagnostic about native output the adapter refused: not
            // evidence of anything, so it changes no turn state.
            AgentEvent::ProtocolError { .. } => TurnStep::Continue,
            AgentEvent::TurnCompleted { is_error: true, .. } => {
                self.cutoff_confirmed = self.cutoff_pending;
                self.cutoff_pending = false;
                self.errored = true;
                self.waiting_for_report = false;
                TurnStep::EndedWithError
            }
            AgentEvent::TurnCompleted {
                is_error: false, ..
            } if self.reported => {
                self.cutoff_pending = false;
                self.completed = true;
                self.waiting_for_report = false;
                TurnStep::Completed
            }
            AgentEvent::TurnCompleted {
                is_error: false, ..
            } => {
                self.cutoff_pending = false;
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

    /// The turn ended without reporting and the CLI says background work is
    /// still pending (#271): it is waiting on it, not silent.
    fn waiting_on_jobs(&self) -> bool {
        self.waiting_for_report
            && !self.gave_up
            && self.grace_deadline.is_none()
            && self.background_pending
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

/// Records that a turn has started waiting on its background work (#271).
async fn note_job_wait_entered(
    pool: &SqlitePool,
    session_id: &str,
    turn: &SingleShotTurn,
    limit: Duration,
    remaining: Duration,
    events_notify: &Notify,
) {
    append_session_note(
        pool,
        session_id,
        "job_wait",
        &job_wait_entered_message(turn, limit, remaining, Utc::now()),
        events_notify,
    )
    .await;
}

/// The wait-entry note's text. The deadline is what is left of the turn's
/// total wait budget if the turn keeps waiting; it is not a promise that the
/// process closes then, because main execution resuming ends the wait.
fn job_wait_entered_message(
    turn: &SingleShotTurn,
    limit: Duration,
    remaining: Duration,
    now: chrono::DateTime<Utc>,
) -> String {
    let deadline = chrono::Duration::from_std(remaining)
        .ok()
        .and_then(|left| now.checked_add_signed(left))
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "never".to_string());
    format!(
        "the turn ended without an accepted report while {}; not nudging while that is \
         pending. If it keeps waiting, its wait budget (the {} limit is a total for the turn, \
         {} of it left) runs out at {deadline}; if the agent starts working again first, the \
         wait ends and the rest of the budget stays for a later wait",
        describe_background_work(&turn.background_jobs, turn.background_pending),
        describe_limit(limit),
        describe_limit(remaining),
    )
}

/// The exhaustion note's text: the turn's total wait budget ran out while it
/// waited, or earlier waits had already used it up before this one began.
fn job_wait_exhausted_message(
    turn: &SingleShotTurn,
    limit: Duration,
    already_used_up: bool,
) -> String {
    let when = if already_used_up {
        "had already been used up by its earlier waits"
    } else {
        "was exhausted while it waited"
    };
    format!(
        "the turn ended without an accepted report and was still waiting ({}) when the \
         turn's job-wait budget of {} (a total per turn) {when}; closing it",
        describe_background_work(&turn.background_jobs, turn.background_pending),
        describe_limit(limit)
    )
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
    let message = job_wait_exhausted_message(turn, turn_timers.job_wait_limit, already_used_up);
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
///
/// A run the claude adapter ended because the role's `claude_settings` were
/// not applied (`ClaudeSettingsRejected` / `ClaudeSettingsUnconfirmed`, #346)
/// ranks after the requested ends (Cancelled, Paused, DaemonStopped) and ahead
/// of every observed one: the turn ended because its settings were not
/// applied, so nothing else can describe this run.
#[cfg(test)]
fn final_run_state(
    kind: SessionKind,
    turn: &SingleShotTurn,
    clean_exit: bool,
    reaped: bool,
    cancelled: bool,
    stopping: bool,
    cleanup: Option<SessionCleanup>,
) -> (SessionStatus, Option<SessionEndReason>) {
    final_run_state_ranked(
        kind, turn, clean_exit, reaped, cancelled, false, stopping, cleanup,
    )
}

/// [`final_run_state`] with the pause request: Cancelled > Paused >
/// DaemonStopped. A single-shot turn that had not completed ends
/// `(Exited, Paused)`; a completed one keeps its ordinary result. A
/// standing session is never stopped by a pause.
#[allow(clippy::too_many_arguments)]
fn final_run_state_ranked(
    kind: SessionKind,
    turn: &SingleShotTurn,
    clean_exit: bool,
    reaped: bool,
    cancelled: bool,
    paused: bool,
    stopping: bool,
    cleanup: Option<SessionCleanup>,
) -> (SessionStatus, Option<SessionEndReason>) {
    use SessionEndReason::{
        Cancelled, DaemonStopped, Interrupted, Lingered, NoReport, Paused, Reaped,
    };
    // Observed ends, ranked after the requested ones (Cancelled, Paused,
    // DaemonStopped) and ahead of every other observed end: the turn ended
    // because its settings were not applied, so nothing else can describe
    // this run.
    let settings_end = turn.settings_end;
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
            if let Some(reason) = settings_end {
                return (Exited, Some(reason));
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
            } else if paused && !turn.completed {
                (Exited, Some(Paused))
            } else if stopping && !turn.completed {
                (Exited, Some(DaemonStopped))
            } else if let Some(reason) = settings_end {
                (Exited, Some(reason))
            } else if turn.gave_up {
                (Exited, Some(NoReport))
            } else if reaped && !turn.completed {
                // The idle reaper closed a stalled turn, which the engine's
                // watcher parks on whatever the status.
                let status = if clean_exit { Idle } else { Exited };
                (status, Some(Reaped))
            } else if turn.lingered {
                // Killing what the agent left behind is bad news only when
                // the cleanup could not be verified. With it verified, a turn
                // that had reported and ended cleanly is complete, and one
                // whose main agent was cut off by a usage limit with nothing
                // after it is the interruption it would have been had it left
                // nothing running. Nothing else is promoted: not a report
                // without a clean end, not a limit the agent recovered from.
                if cleanup == Some(SessionCleanup::Verified) && turn.completed {
                    (Idle, None)
                } else if cleanup == Some(SessionCleanup::Verified) && turn.cutoff_confirmed {
                    (Exited, Some(Interrupted))
                } else {
                    (Exited, Some(Lingered))
                }
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

/// A test hook run before each lock of the tracker's state.
#[cfg(test)]
type LockHook = Box<dyn Fn(&std::sync::Mutex<TrackerState>) + Send + Sync>;

/// What a turn's kills and sweeps found, collected from every site that
/// signals the agent's group (`cancel`, `shutdown`, the grace branch, the
/// tail) and reported once by `drain_session`.
struct LeftoverTracker {
    marker: String,
    kind: SessionKind,
    reader: ProcReader,
    state: std::sync::Mutex<TrackerState>,
    /// Test hook: runs at the start of every [`Self::state`] call, before the
    /// lock is taken, so a test can look at the tracker where another writer
    /// could run between two acquisitions.
    #[cfg(test)]
    before_lock: std::sync::OnceLock<LockHook>,
}

#[derive(Default)]
struct TrackerState {
    /// Identities of processes proven to descend from the agent.
    recorded: std::collections::HashSet<Identity>,
    /// Processes the sweeps delivered SIGKILL to by pid. A delivered signal
    /// is not a death: a pid can be here and in `survivors` at once.
    killed: Vec<(i32, String)>,
    /// Processes the last completed check still saw alive (or a failed kill
    /// left), each once per pid.
    survivors: Vec<(i32, String)>,
    /// The start time of each pid in `survivors`, which together form that
    /// process's identity. Always holds exactly the pids in `survivors`.
    survivor_starts: HashMap<i32, u64>,
    /// The start time of each pid in `killed`, taken from the scan that
    /// selected it. With the pid it is the signalled process's identity, so
    /// a final observation can tell that it is still alive even when the
    /// selector no longer owns it. Holds exactly the pids in `killed`.
    killed_starts: HashMap<i32, u64>,
    /// Every failed process-table read of the turn. Sticky: a later read
    /// that works cannot recover what an earlier one did not show.
    scan_failures: Vec<String>,
    /// Failed signals to the agent's group, as `"{site}: {error}"`.
    group_kill_errors: Vec<String>,
    /// Pids counted towards the turn's grace decision: the first successful
    /// by-pid delivery at each, before a sweep's original stop point, at any
    /// cleanup site, and the earlier sites' extra-round deliveries that the
    /// grace site carried on entry (`carry_late_kills`, which may be gone by
    /// then). Pid-based on purpose (a reused pid already counted stays
    /// counted), turn-wide, and separate from `killed`, which lists every
    /// delivery for diagnostics, late ones included. It only deduplicates
    /// policy counts and never says whether a process is alive.
    policy_counted_pids: std::collections::HashSet<i32>,
    /// Pids a sweep signalled successfully only in its extra observation
    /// rounds, that no site had policy-counted at that time, each once, in
    /// the order signalled. The grace site takes them once on entry
    /// ([`TrackerState::carry_late_kills`]) so a process an earlier cleanup
    /// killed cannot erase the lingering result by having really exited by
    /// then. History only: it never says whether a process is alive and is
    /// never signalled from.
    late_killed_pids: Vec<i32>,
}

/// What [`kill_and_sweep`] did at one site.
struct KillReport {
    group: GroupKill,
    /// Processes the sweeps of this call delivered SIGKILL to by pid, before
    /// a sweep's original stop point, and that no earlier site of the turn
    /// had counted (the turn's `policy_counted_pids`). A delivery that only
    /// the extra observation rounds made is listed in `killed` for
    /// diagnostics but is not counted by that call; the grace site adds
    /// the ones earlier sites made that way (`carry_late_kills`). A
    /// delivered signal is not a confirmed death. When the group kill was
    /// delivered, the agent's group members are not counted: they are the
    /// group kill's. When it was not, the sweeps signal them by pid and they
    /// are counted.
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
        GroupKill::Killed(n) => format!("signalled {n} live member(s)"),
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

/// The `lingered` note when the group had nothing alive and the only count
/// is an earlier cleanup's signal delivery, still counted as left running.
/// Claims a delivery, never a death: a deduplicated count of zero at grace
/// does not show that grace's own sweep saw or signalled nothing (it may
/// have signalled the carried process again, or failed to read the table).
fn lingered_carried_message(grace_s: f32, after: &str) -> String {
    format!(
        "the agent's process group had exited {grace_s:.1}s after {after}; an earlier \
         cleanup of this turn sent SIGKILL to a process it started"
    )
}

/// The `lingered` note for a grace-site kill: what the pre-kill scan saw, and
/// what was done about it. Claims an attempt, never a death.
fn lingered_message(group: &GroupKill, grace_s: f32, after: &str) -> String {
    match group {
        GroupKill::Killed(n) => format!(
            "{n} process(es) of the agent's process group were still running {grace_s:.1}s \
             after {after}; sent SIGKILL to the group"
        ),
        GroupKill::Failed(err) => format!(
            "processes of the agent's process group were still running {grace_s:.1}s after \
             {after}; could not signal the group: {err}"
        ),
        GroupKill::Unknown {
            signal_result: Ok(()),
            ..
        } => format!(
            "{grace_s:.1}s after {after}, could not check whether the agent's process group \
             was still running; sent SIGKILL to the group"
        ),
        GroupKill::Unknown {
            signal_result: Err(err),
            ..
        } => format!(
            "{grace_s:.1}s after {after}, could not check whether the agent's process group \
             was still running, and could not signal the group: {err}"
        ),
        GroupKill::NothingAlive => format!(
            "the agent's process group had exited {grace_s:.1}s after {after}, but processes \
             it started were still running; sent them SIGKILL"
        ),
    }
}

impl TrackerState {
    /// Takes the pids earlier sites signalled in their extra rounds and
    /// returns how many no site had policy-counted, marking each as counted
    /// in the same step so that grace's own ordinary count of a still-live
    /// one does not count it a second time. Called once, on entry to the
    /// grace site, so the extra-round finds of grace itself and the tail's
    /// are never carried. A carried pid may be gone or a zombie by now; it
    /// is counted, never signalled.
    fn carry_late_kills(&mut self) -> usize {
        std::mem::take(&mut self.late_killed_pids)
            .into_iter()
            .filter(|pid| self.policy_counted_pids.insert(*pid))
            .count()
    }

    /// Records a process a check still found alive, once per pid. A signal
    /// sent earlier does not excuse it: `killed` lists signals the sweep
    /// delivered and `survivors` lists what a check still saw, so one pid can
    /// be in both. `write_leftover_notes` then names it as still running and
    /// never as killed.
    fn add_survivor(&mut self, pid: i32, comm: String, start: u64) {
        if !self.survivors.iter().any(|(p, _)| *p == pid) {
            self.survivors.push((pid, comm));
            self.survivor_starts.insert(pid, start);
        }
    }

    /// Applies one completed final observation. `table` is the scan that
    /// ended the sweep and `live` the pids in it the check calls alive.
    ///
    /// A survivor listed earlier stays only while `table` still has a live
    /// (non-zombie) entry with the same pid *and* start time: that is the
    /// same process, even if the selector no longer owns it. A pid that is
    /// gone, a zombie, or reused by another process clears it. Then every
    /// pid in `live` is added with this scan's command and start. Vector and
    /// map change together, under the caller's one lock.
    ///
    /// Returns how many survivors that leaves: the observed-live set that the
    /// last-check warning, the survivor list and the note all report.
    fn settle_survivors(&mut self, table: &[ProcEntry], live: &[i32]) -> usize {
        let starts = std::mem::take(&mut self.survivor_starts);
        self.survivors.retain(|(pid, _)| {
            starts.get(pid).is_some_and(|start| {
                table
                    .iter()
                    .any(|e| e.pid == *pid && e.start == *start && !e.zombie)
            })
        });
        self.survivor_starts = starts
            .into_iter()
            .filter(|(pid, _)| self.survivors.iter().any(|(p, _)| p == pid))
            .collect();
        for &pid in live {
            if let Some(entry) = table.iter().find(|e| e.pid == pid) {
                self.add_survivor(pid, entry.comm.clone(), entry.start);
            }
        }
        // A process SIGKILL was delivered to is a survivor if this scan still
        // has that identity alive, even when the selector no longer owns it
        // (it dropped its marker, say): the signal is not a death.
        let still_alive: Vec<(i32, String, u64)> = self
            .killed
            .iter()
            .filter_map(|(pid, comm)| {
                let start = *self.killed_starts.get(pid)?;
                table
                    .iter()
                    .any(|e| e.pid == *pid && e.start == start && !e.zombie)
                    .then(|| (*pid, comm.clone(), start))
            })
            .collect();
        for (pid, comm, start) in still_alive {
            self.add_survivor(pid, comm, start);
        }
        self.survivors.len()
    }

    /// Records a failed signal to the agent's group, once, for the site that
    /// sent it: `Failed`, or `Unknown` whose signal failed for any reason but
    /// `ESRCH` (the group was already gone). Records an error only: whether
    /// anything survived is for the final observation to say.
    fn record_group_kill_error(&mut self, site: &str, group: &GroupKill) {
        let err = match group {
            GroupKill::Failed(err) => err,
            GroupKill::Unknown {
                signal_result: Err(err),
                ..
            } if err.raw_os_error() != Some(libc::ESRCH) => err,
            GroupKill::Killed(_) | GroupKill::NothingAlive | GroupKill::Unknown { .. } => return,
        };
        self.group_kill_errors.push(format!("{site}: {err}"));
    }
}

impl LeftoverTracker {
    fn new(marker: String, kind: SessionKind, reader: ProcReader) -> Self {
        Self {
            marker,
            kind,
            reader,
            state: Default::default(),
            #[cfg(test)]
            before_lock: Default::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        #[cfg(test)]
        if let Some(hook) = self.before_lock.get() {
            hook(&self.state);
        }
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
    /// Returns how many processes it delivered SIGKILL to by pid that the
    /// turn had not yet counted (`policy_counted_pids`), in the rounds up to
    /// the one where nothing owned was left to signal or watch (what the loop
    /// always did). A process found and signalled in the rounds that only
    /// wait for the leader or group is listed in `killed` (diagnostics) but
    /// neither counted nor entered in the policy history; it goes to
    /// `late_killed_pids`, which the grace site carries once on entry. A
    /// delivered signal is not a death, so the count is
    /// not a count of deaths.
    ///
    /// The scan that ends the loop is the *final observation*: it is the one
    /// thing that says what is still running. It covers the agent, its
    /// process group (`group_live`, observed but never signalled by pid) and
    /// every owned process, including one whose kill failed. It settles the
    /// turn's survivors (see [`TrackerState::settle_survivors`]), also when
    /// it finds nothing. A scan that fails is not an observation: it records
    /// a failure and leaves earlier survivors as they were.
    ///
    /// `spare_group` is the process group whose SIGKILL `killpg` already
    /// delivered (see [`kill_and_sweep`]). An owned entry in that group is
    /// watched, not signalled and not recorded as killed: it still keeps the
    /// loop going, and if it is still in the table at the final observation
    /// it is listed as a survivor. `None` signals every owned entry.
    async fn sweep(&self, agent: i32, spare_group: Option<i32>, site: &str) -> usize {
        // SAFETY: both only read process state.
        let (daemon_sid, uid) = unsafe { (libc::getsid(0), libc::geteuid()) };
        let mut failed = std::collections::HashSet::new();
        let mut killed_here = 0usize;
        // Every pid this sweep newly signalled, for the warning below. Unlike
        // `killed_here` it also counts the rounds past the old stop point.
        let mut delivered_here = 0usize;
        // Set once a round has had nothing owned left to signal or watch: the
        // point where this loop used to stop. The rounds after it exist only
        // to observe a leader or group member that is still exiting. They
        // still signal an owned process they find, but those signals are not
        // counted in the returned total, which `lingered_decision` reads: more
        // looking must not change what the grace cleanup means for the turn.
        let mut past_original_stop = false;
        for round in 0..=SWEEP_ROUNDS {
            let table = match self.scan(true).await {
                Ok(table) => table,
                Err(err) => {
                    self.note_scan_failure(site, &err);
                    return killed_here;
                }
            };
            let recorded = self.state().recorded.clone();
            let all_owned = proc_table::owned_pids(
                &table,
                &OwnershipInput {
                    agent: Some(agent),
                    recorded: &recorded,
                    daemon_pid: std::process::id() as i32,
                    daemon_sid,
                    uid,
                },
            );
            // The agent and the members of its group that are still alive.
            // Only observed and waited for, never added to `to_kill`.
            let group_live: Vec<i32> = table
                .iter()
                .filter(|e| (e.pid == agent || e.pgid == agent) && !e.zombie)
                .map(|e| e.pid)
                .collect();
            let owned: Vec<i32> = all_owned
                .iter()
                .copied()
                .filter(|pid| !failed.contains(pid))
                .collect();
            let in_spared_group = |pid: i32| {
                spare_group
                    .is_some_and(|group| table.iter().any(|e| e.pid == pid && e.pgid == group))
            };
            let (watched, to_kill): (Vec<i32>, Vec<i32>) =
                owned.into_iter().partition(|pid| in_spared_group(*pid));
            let comm = |pid: i32| {
                table
                    .iter()
                    .find(|e| e.pid == pid)
                    .map(|e| e.comm.clone())
                    .unwrap_or_default()
            };
            let start = |pid: i32| table.iter().find(|e| e.pid == pid).map_or(0, |e| e.start);
            let group_waiting = group_live.iter().any(|pid| !failed.contains(pid));
            if (watched.is_empty() && to_kill.is_empty() && !group_waiting) || round == SWEEP_ROUNDS
            {
                // The final observation: everything owned (a failed kill
                // included) and everything alive in the agent's group.
                let mut live = all_owned;
                for pid in group_live {
                    if !live.contains(&pid) {
                        live.push(pid);
                    }
                }
                // One lock settles the survivors and says how many there are,
                // so the warning counts what the list and the note report:
                // also a signalled identity the selector no longer owns.
                let still_running = self.state().settle_survivors(&table, &live);
                if round == SWEEP_ROUNDS && still_running > 0 {
                    tracing::warn!(
                        site,
                        count = still_running,
                        "still running at the sweep's last check"
                    );
                }
                break;
            }
            if watched.is_empty() && to_kill.is_empty() {
                past_original_stop = true;
            }
            // Pid reuse between the scan and this kill is not guarded beyond
            // the scan: both kernels allocate pids sequentially, so it would
            // take a full wraparound inside this window. Accepted cost.
            for pid in to_kill {
                match proc_table::kill_pid(pid) {
                    proc_table::PidKill::Killed => {
                        // Delivered, not confirmed: only a later final
                        // observation can clear a survivor.
                        let mut state = self.state();
                        // The latest signalled identity at this pid, listed
                        // before or not: a reused pid signalled again is a
                        // new process, and the final scan must match it.
                        state.killed_starts.insert(pid, start(pid));
                        if let Some(entry) = state.killed.iter_mut().find(|(p, _)| *p == pid) {
                            entry.1 = comm(pid);
                        } else {
                            state.killed.push((pid, comm(pid)));
                            delivered_here += 1;
                        }
                        // The grace decision counts a pid once per turn, at
                        // any site, and only before the original stop point.
                        // `killed` above is diagnostics: a delivery made only
                        // by the extra rounds is listed there without using
                        // up the pid's count at a later site.
                        if !past_original_stop {
                            if state.policy_counted_pids.insert(pid) {
                                killed_here += 1;
                            }
                        } else if !state.policy_counted_pids.contains(&pid)
                            && !state.late_killed_pids.contains(&pid)
                        {
                            // Kept for the grace site, which counts it even
                            // if the process has exited by then.
                            state.late_killed_pids.push(pid);
                        }
                    }
                    proc_table::PidKill::Gone => {}
                    proc_table::PidKill::Failed(err) => {
                        tracing::error!(site, pid, %err, "could not kill a leftover process");
                        failed.insert(pid);
                        self.state().add_survivor(pid, comm(pid), start(pid));
                    }
                }
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
        if delivered_here > 0 {
            tracing::warn!(
                site,
                signalled = delivered_here,
                "sent SIGKILL to processes left behind by the turn"
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
///
/// A signal is not a death. After the kill, the sweep reads the process
/// table again, and what it still sees alive is what the turn's notes name
/// as "still running at the last check": the agent, its process group and the
/// processes the selector ([`proc_table::owned_pids`]) can prove belong to
/// the turn. A failed group signal is recorded as an error, but it does not
/// by itself make the group a survivor; the final observation decides. No
/// task outcome depends on any of this.
///
/// # What the sweep leaves to the group kill
///
/// When `killpg` returned 0, SIGKILL was delivered to every member of the
/// agent's group, and it cannot be caught or ignored. A member may still be in
/// the sweep's table, not yet a zombie, with the signal pending; signalling it
/// again by pid would succeed (`kill` also returns 0 for an unreaped
/// zombie) and the turn would get a false `leftovers_killed` note for a
/// process the group kill had already signalled. So the sweep spares owned
/// entries whose `pgid` is the agent's: it does not signal them, does not
/// record them as killed, but still watches them, and lists one still in the
/// table at the final observation as a survivor. A process that left the
/// group (`setpgid`, `setsid`) has another pgid and is signalled and recorded
/// as before.
///
/// When `killpg` did not deliver (`GroupKill::Failed`, `Unknown` with an
/// `Err` signal result, `NothingAlive` from `ESRCH`/`EPERM`, or the early
/// returns where it is never called) nothing is spared: the sweep signals
/// same-group entries by pid, as it did before this rule.
///
/// One tracker sees up to three sites in a turn (cancel, shutdown, settings
/// or grace, then always the end of the turn). A member spared at one site is
/// gone by the next, or a zombie (never owned), or still alive, in which case
/// `killpg` returns 0 again and it is spared again; it is never recorded as
/// killed later. The tail's observation can clear a survivor an earlier site
/// listed.
///
/// Accepted blind spots, in both of which the process is spared, watched, and
/// listed as "still running at the last check" if still alive at the final
/// observation, so the note shows it instead of signalling it:
/// - (a) A process that enters the agent's group after `killpg` returned.
///   Only `setpgid` from a process in the daemon's session can do that.
/// - (b) A child whose fork completes while `killpg` is walking the group may
///   not receive the signal. On macOS this is unverified; Linux delivers
///   group signals to a child being forked.
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
            group: crate::shell::kill_group_with(pgid, live).0,
            sweep_killed: 0,
        };
    }
    match &scan {
        Ok(table) => tracker.record_table(table, agent),
        Err(err) => tracker.note_scan_failure(site, err),
    }
    let (group, delivered) = crate::shell::kill_group_with(pgid, live);
    tracker.state().record_group_kill_error(site, &group);
    let sweep_killed = tracker.sweep(agent, delivered.then_some(agent), site).await;
    tracing::debug!(site, group = %describe_group_kill(&group), delivered, sweep_killed, "tried to signal the agent's group and swept");
    KillReport {
        group,
        sweep_killed,
    }
}

/// The text of the `leftovers_killed` note, or `None` when there is nothing
/// to say, and of the `leftovers_unchecked` note, from what a turn's cleanup
/// recorded.
///
/// A pid in both `killed` and `survivors` is named once, as still running.
/// Every list is sorted by pid. Group-kill errors appear only beside a
/// survivor or a failed scan: on their own they do not say anything was left
/// running.
fn leftover_note_texts(
    killed: &[(i32, String)],
    survivors: &[(i32, String)],
    failures: &[String],
    group_errors: &[String],
) -> (Option<String>, Option<String>) {
    let list = |items: &mut Vec<&(i32, String)>| {
        items.sort_by_key(|(pid, _)| *pid);
        items
            .iter()
            .map(|(pid, comm)| format!("{pid} ({comm})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let group_failed = || {
        format!(
            "the kill of the agent's process group failed: {}",
            group_errors.join("; ")
        )
    };
    let mut killed_only: Vec<&(i32, String)> = killed
        .iter()
        .filter(|(pid, _)| !survivors.iter().any(|(p, _)| p == pid))
        .collect();
    let mut still_running: Vec<&(i32, String)> = survivors.iter().collect();
    let mut parts = Vec::new();
    if !killed_only.is_empty() {
        let n = killed_only.len();
        let list = list(&mut killed_only);
        parts.push(if failures.is_empty() {
            format!("killed {n} process(es) the turn left running: {list}")
        } else {
            format!(
                "sent SIGKILL to {n} process(es) the turn left running, but could not confirm \
                 they are gone: {list}"
            )
        });
    }
    if !still_running.is_empty() {
        parts.push(format!(
            "{} still running at the last check: {}",
            still_running.len(),
            list(&mut still_running)
        ));
        if !group_errors.is_empty() {
            parts.push(group_failed());
        }
    }
    let killed_note = (!parts.is_empty()).then(|| parts.join("; "));
    let unchecked_note = (!failures.is_empty()).then(|| {
        let mut message = format!(
            "the process table could not be read, so nothing confirms the turn's processes \
             are gone: {}",
            failures.join("; ")
        );
        if !group_errors.is_empty() {
            message.push_str("; ");
            message.push_str(&group_failed());
        }
        message
    });
    (killed_note, unchecked_note)
}

/// Writes the `leftovers_killed` and `leftovers_unchecked` notes, once, at the
/// end of the turn.
///
/// Returns whether the observation was clean: no survivor in the last check
/// and no failed process-table read anywhere in the turn. Computed from the
/// same vectors the notes are built from, taken under the one lock below, so
/// it needs no further read. A failed note append does not change it, and
/// group-signal errors alone do not either.
async fn write_leftover_notes(
    pool: &SqlitePool,
    session_id: &str,
    tracker: &LeftoverTracker,
    events_notify: &Notify,
) -> bool {
    // One lock for everything consumed, so the survivors and their identities
    // leave together.
    let (killed, survivors, failures, group_errors) = {
        let mut state = tracker.state();
        state.survivor_starts.clear();
        state.killed_starts.clear();
        state.policy_counted_pids.clear();
        state.late_killed_pids.clear();
        (
            std::mem::take(&mut state.killed),
            std::mem::take(&mut state.survivors),
            std::mem::take(&mut state.scan_failures),
            std::mem::take(&mut state.group_kill_errors),
        )
    };
    let observed_clean = survivors.is_empty() && failures.is_empty();
    if !observed_clean {
        tracing::warn!(
            session_id,
            still_running = survivors.len(),
            failed_scans = failures.len(),
            "the turn's cleanup is not verified: a process was still running at the last check, or the process table could not be read"
        );
    }
    let (killed_note, unchecked_note) =
        leftover_note_texts(&killed, &survivors, &failures, &group_errors);
    if let Some(message) = killed_note {
        append_session_note(
            pool,
            session_id,
            "leftovers_killed",
            &message,
            events_notify,
        )
        .await;
    }
    if let Some(message) = unchecked_note {
        append_session_note(
            pool,
            session_id,
            "leftovers_unchecked",
            &message,
            events_notify,
        )
        .await;
    }
    observed_clean
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

    /// A standing (chat-shaped) session row: tracks no cleanup.
    async fn seed_session(pool: &SqlitePool) -> String {
        seed_session_for(pool, SessionKind::Standing).await
    }

    /// A row as `launch_turn` creates it: a single-shot session starts out
    /// with cleanup pending, a standing one with none.
    async fn seed_session_for(pool: &SqlitePool, kind: SessionKind) -> String {
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
                closes: &[],
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
                cleanup: (kind == SessionKind::SingleShot).then_some(SessionCleanup::Pending),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn role_config() -> RoleConfig {
        RoleConfig {
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            role: "coder".to_string(),
            project_id: None,
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
        sessions::update_status(&pool, &session_id, SessionStatus::Idle, None, None, None)
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
        sessions::update_status(&pool, &session_id, SessionStatus::Idle, None, None, None)
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
        let heartbeat = dir.join("heartbeat");
        let child_pid = dir.join("child.pid");
        let wrapper = dir.join("fake-claude-spawns-child");
        crate::test_support::write_executable(
            &wrapper,
            format!(
                "#!/bin/sh\nCHOCO_TEST_HEARTBEAT='{}' CHOCO_TEST_CHILD_PID='{}' CHOCO_TEST_AGENT_PID='{}' exec '{}' \"$@\"\n",
                heartbeat.display(),
                child_pid.display(),
                dir.join("agent.pid").display(),
                fixture_binary("fake_claude_spawns_child.py"),
            ),
        );
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
        let script = dir.join("script.json");
        std::fs::write(&script, steps.to_string()).unwrap();
        let wrapper = dir.join("fake-claude-script");
        crate::test_support::write_executable(
            &wrapper,
            format!(
                "#!/bin/sh\nFAKE_CLAUDE_SCRIPT='{}' exec '{}' \"$@\"\n",
                script.display(),
                fixture_binary("fake_claude_script.py"),
            ),
        );
        wrapper.display().to_string()
    }

    async fn start_single_shot(
        binary: String,
        timers: TurnTimers,
    ) -> (SqlitePool, String, Arc<SessionManager>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
            pending: n > 0,
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
            pending: true,
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
            t.background_pending = true;
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
        turn.background_pending = false;
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
        turn.background_pending = false;
        let t4 = t0 + StdDuration::from_secs(4);
        assert_eq!(turn.update_job_wait(t4, limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_spent, StdDuration::from_secs(4));

        // A new wait gets what is left, not a fresh limit.
        turn.background_jobs = vec![a_job()];
        turn.background_pending = true;
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
        turn.background_pending = false;
        let t11 = t5 + StdDuration::from_secs(6);
        turn.update_job_wait(t11, limit);
        assert_eq!(turn.job_wait_spent, limit);

        // Entering with nothing left is refused, and `spent` is not reset.
        turn.background_jobs = vec![a_job()];
        turn.background_pending = true;
        assert_eq!(
            turn.update_job_wait(t11, limit),
            JobWaitChange::ExhaustedOnEntry
        );
        assert_eq!(turn.job_wait_deadline, None);
        assert_eq!(turn.job_wait_spent, limit);
    }

    // ---- pending work with no job identities (omp) ----

    fn omp_level(pending: bool) -> AgentEvent {
        AgentEvent::BackgroundJobs {
            running: Vec::new(),
            pending,
        }
    }

    fn named_level(pending: bool) -> AgentEvent {
        AgentEvent::BackgroundJobs {
            running: vec![a_job()],
            pending,
        }
    }

    fn clean_end() -> AgentEvent {
        use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};
        AgentEvent::TurnCompleted {
            is_error: false,
            usage: TurnUsage {
                cost_usd: None,
                tokens: TokenCounts::default(),
                models: None,
                wall_time_ms: None,
                model_turns: None,
                billing: BillingMode::Unknown,
                counting: UsageCounting::PerTurn,
            },
        }
    }

    fn error_end() -> AgentEvent {
        match clean_end() {
            AgentEvent::TurnCompleted { usage, .. } => AgentEvent::TurnCompleted {
                is_error: true,
                usage,
            },
            other => other,
        }
    }

    fn limit() -> AgentEvent {
        AgentEvent::Interrupted {
            message: "limit".into(),
            detected_by: crate::adapter::InterruptionEvidence::Structured,
        }
    }

    fn helper(event: AgentEvent) -> AgentEvent {
        AgentEvent::Subagent {
            parent_tool_use_id: "toolu_helper".into(),
            event: Box::new(event),
        }
    }

    fn main_activity() -> Vec<(&'static str, AgentEvent)> {
        vec![
            (
                "assistant text",
                AgentEvent::AssistantMessage { text: "x".into() },
            ),
            ("thinking", AgentEvent::Thinking { text: "x".into() }),
            (
                "tool call",
                AgentEvent::ToolCall {
                    tool_use_id: "t1".into(),
                    tool: "Bash".into(),
                    input: json!({}),
                },
            ),
            (
                "tool result",
                AgentEvent::ToolResult {
                    tool_use_id: "t1".into(),
                    tool: "Bash".into(),
                    output: "ok".into(),
                    is_error: false,
                },
            ),
            ("execution started", AgentEvent::ExecutionStarted),
        ]
    }

    /// A limit confirms a cutoff only when nothing the main agent did comes
    /// after it before the turn's error end. Every category of event is
    /// either activity (clears it), a limit (sets it), an end (consumes it)
    /// or neutral (leaves it), over a mix of the ones that matter.
    #[test]
    fn a_cutoff_is_confirmed_only_by_a_limit_with_no_main_work_after_it() {
        let observe = |events: Vec<AgentEvent>| {
            let mut turn = SingleShotTurn::default();
            for event in &events {
                turn.observe(event);
            }
            turn
        };
        // The production order: the limit message, the interruption, the end.
        let cut_off = observe(vec![
            AgentEvent::AssistantMessage {
                text: "limit".into(),
            },
            limit(),
            error_end(),
        ]);
        assert!(cut_off.cutoff_confirmed && !cut_off.cutoff_pending);

        // Main activity after the limit clears it, each kind alone.
        for (name, activity) in main_activity() {
            let turn = observe(vec![limit(), activity, error_end()]);
            assert!(!turn.cutoff_confirmed, "{name} after the limit");
            // And activity before the limit does not.
            let (_, activity) = main_activity()
                .into_iter()
                .find(|(n, _)| *n == name)
                .unwrap();
            let turn = observe(vec![activity, limit(), error_end()]);
            assert!(turn.cutoff_confirmed, "{name} before the limit");
        }

        // Neutral main events and anything a helper does leave it alone.
        let neutral = vec![
            AgentEvent::SessionMeta {
                adapter_session_id: "s".into(),
                details: json!({}),
            },
            omp_level(true),
            AgentEvent::ProtocolError {
                message: "x".into(),
            },
            AgentEvent::Error {
                message: "x".into(),
            },
            helper(AgentEvent::AssistantMessage { text: "x".into() }),
            helper(AgentEvent::ExecutionStarted),
            helper(error_end()),
            helper(limit()),
        ];
        for event in neutral {
            let turn = observe(vec![limit(), event.clone(), error_end()]);
            assert!(turn.cutoff_confirmed, "{event:?} must not clear the limit");
            let turn = observe(vec![event.clone(), error_end()]);
            assert!(!turn.cutoff_confirmed, "{event:?} must not set a limit");
        }

        // A clean end consumes it, with and without a report before it, and a
        // later error end then confirms nothing.
        for reported in [false, true] {
            let mut events = Vec::new();
            if reported {
                events.push(AgentEvent::ToolCall {
                    tool_use_id: "r1".into(),
                    tool: chocofactory_core::mcp::qualified_report_outcome_tool_name(),
                    input: json!({}),
                });
                events.push(AgentEvent::ToolResult {
                    tool_use_id: "r1".into(),
                    tool: "report_outcome".into(),
                    output: "ok".into(),
                    is_error: false,
                });
            }
            events.extend([limit(), clean_end(), error_end()]);
            let turn = observe(events);
            assert!(!turn.cutoff_confirmed, "reported: {reported}");
        }

        // A closed turn never acquires a cutoff, and one it had keeps it.
        let errored_then_limit = observe(vec![error_end(), limit(), error_end()]);
        assert!(!errored_then_limit.cutoff_confirmed && !errored_then_limit.cutoff_pending);
        let completed_then_limit = observe(vec![
            AgentEvent::ToolCall {
                tool_use_id: "r1".into(),
                tool: chocofactory_core::mcp::qualified_report_outcome_tool_name(),
                input: json!({}),
            },
            AgentEvent::ToolResult {
                tool_use_id: "r1".into(),
                tool: "report_outcome".into(),
                output: "ok".into(),
                is_error: false,
            },
            clean_end(),
            limit(),
            error_end(),
        ]);
        assert!(completed_then_limit.completed);
        assert!(!completed_then_limit.cutoff_confirmed && !completed_then_limit.cutoff_pending);
        let twice = observe(vec![limit(), error_end(), limit(), error_end()]);
        assert!(twice.cutoff_confirmed);
        let mut gave_up = turn_with(|t| t.gave_up = true);
        gave_up.observe(&limit());
        gave_up.observe(&error_end());
        assert!(!gave_up.cutoff_confirmed);
    }

    /// A verified cleanup promotes only the two shapes the grace kill would
    /// otherwise have ended `Lingered`, and only when it is exactly verified.
    #[test]
    fn only_a_verified_cleanup_promotes_a_lingering_turn() {
        use SessionEndReason::{Interrupted, Lingered};
        use SessionStatus::{Exited, Idle};
        let completed = turn_with(|t| {
            t.completed = true;
            t.lingered = true;
        });
        let cutoff = turn_with(|t| {
            t.errored = true;
            t.interrupted = true;
            t.cutoff_confirmed = true;
            t.lingered = true;
        });
        // Interrupted earlier, recovered, then a generic error end.
        let recovered = turn_with(|t| {
            t.errored = true;
            t.interrupted = true;
            t.lingered = true;
        });
        // Reported but never cleanly ended.
        let reported_only = turn_with(|t| {
            t.reported = true;
            t.errored = true;
            t.lingered = true;
        });
        let rank = |turn: &SingleShotTurn, cleanup| {
            final_run_state(
                SessionKind::SingleShot,
                turn,
                false,
                false,
                false,
                false,
                cleanup,
            )
        };
        for cleanup in [
            None,
            Some(SessionCleanup::Pending),
            Some(SessionCleanup::Unverified),
        ] {
            assert_eq!(
                rank(&completed, cleanup),
                (Exited, Some(Lingered)),
                "{cleanup:?}"
            );
            assert_eq!(
                rank(&cutoff, cleanup),
                (Exited, Some(Lingered)),
                "{cleanup:?}"
            );
        }
        let verified = Some(SessionCleanup::Verified);
        assert_eq!(rank(&completed, verified), (Idle, None));
        assert_eq!(rank(&cutoff, verified), (Exited, Some(Interrupted)));
        assert_eq!(rank(&recovered, verified), (Exited, Some(Lingered)));
        assert_eq!(rank(&reported_only, verified), (Exited, Some(Lingered)));
    }

    /// The signal, not the list, decides whether the turn is waiting: pending
    /// work with no identities waits (named Claude jobs too), the list alone
    /// does not.
    #[test]
    fn pending_work_waits_whether_or_not_the_cli_names_it() {
        for (event, waits) in [
            (omp_level(true), true),
            (named_level(true), true),
            (omp_level(false), false),
            (named_level(false), false),
        ] {
            let mut turn = SingleShotTurn::default();
            turn.observe(&event);
            turn.observe(&clean_end());
            assert!(turn.waiting_for_report);
            assert_eq!(turn.waiting_on_jobs(), waits, "{event:?}");
            assert_eq!(turn.should_nudge(), !waits, "{event:?}");
        }
    }

    /// A deterministic timeline of the one budget: a 4 s wait, 7 s of active
    /// work, then a wait that has only the 6 s that are left.
    #[test]
    fn omp_pending_wait_uses_the_existing_budget() {
        let limit = StdDuration::from_secs(10);
        let t0 = tokio::time::Instant::now();
        let at = |secs: u64| t0 + StdDuration::from_secs(secs);
        let mut turn = SingleShotTurn::default();

        // Yield #1: pending work published, then a report-less clean end.
        turn.observe(&omp_level(true));
        assert!(matches!(
            turn.observe(&clean_end()),
            TurnStep::WaitingForReport
        ));
        assert!(turn.waiting_on_jobs());
        assert!(!turn.should_nudge(), "pending work is never nudged");
        assert_eq!(
            turn.update_job_wait(t0, limit),
            JobWaitChange::Entered { remaining: limit }
        );

        // 4 s later the main agent starts: that alone ends the wait and
        // clears the deadline, with no assistant or tool output required.
        turn.observe(&AgentEvent::ExecutionStarted);
        assert!(!turn.waiting_for_report);
        assert!(!turn.waiting_on_jobs());
        assert_eq!(turn.update_job_wait(at(4), limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_deadline, None);
        assert_eq!(turn.job_wait_spent, StdDuration::from_secs(4));

        // 7 s of silent active work: nothing is spent, nothing is nudged.
        assert!(!turn.should_nudge());
        assert_eq!(turn.update_job_wait(at(11), limit), JobWaitChange::NoChange);
        assert_eq!(
            turn.job_wait_spent,
            StdDuration::from_secs(4),
            "active time is free"
        );

        // Yield #2, pending again: only the remaining 6 s are available.
        turn.observe(&clean_end());
        assert_eq!(
            turn.update_job_wait(at(11), limit),
            JobWaitChange::Entered {
                remaining: StdDuration::from_secs(6)
            }
        );
        assert_eq!(turn.job_wait_deadline, Some(at(17)));
        assert!(!turn.job_wait_expired(at(16)));
        assert!(
            turn.job_wait_expired(at(17)),
            "exhaustion at the remaining budget"
        );

        // Waking again after it all, then waiting a third time: spent only
        // ever rose, so there is nothing left on entry.
        turn.observe(&AgentEvent::ExecutionStarted);
        assert_eq!(turn.update_job_wait(at(17), limit), JobWaitChange::NoChange);
        assert_eq!(turn.job_wait_spent, limit);
        turn.observe(&clean_end());
        assert_eq!(
            turn.update_job_wait(at(18), limit),
            JobWaitChange::ExhaustedOnEntry
        );
        assert_eq!(turn.job_wait_spent, limit);
    }

    /// A wrapped helper's start is not the main agent working again.
    #[test]
    fn a_helpers_start_is_not_main_activity() {
        let mut turn = SingleShotTurn::default();
        turn.observe(&omp_level(true));
        turn.observe(&clean_end());
        turn.observe(&AgentEvent::Subagent {
            parent_tool_use_id: "toolu_h".into(),
            event: Box::new(AgentEvent::ExecutionStarted),
        });
        assert!(turn.waiting_for_report);
        assert!(turn.waiting_on_jobs());
    }

    #[test]
    fn pending_work_notes_do_not_claim_known_job_identities() {
        let limit = StdDuration::from_secs(10);
        let t0 = tokio::time::Instant::now();
        let now = Utc::now();
        let mut turn = SingleShotTurn::default();
        turn.observe(&omp_level(true));
        turn.observe(&clean_end());
        assert!(matches!(
            turn.update_job_wait(t0, limit),
            JobWaitChange::Entered { .. }
        ));
        let entered = job_wait_entered_message(&turn, limit, limit, now);
        assert!(
            entered.contains("pending background work; job list unavailable"),
            "{entered}"
        );
        for banned in ["no background jobs running", "0 background job(s)"] {
            assert!(!entered.contains(banned), "{entered}");
        }
        let payload = omp_level(true).payload();
        assert_eq!(
            payload["message"],
            "pending background work; job list unavailable"
        );
        assert_eq!(payload["pending"], true);
        assert_eq!(payload["running"], json!([]));

        // The deadline is a remaining budget, not a promise to close then.
        assert!(entered.contains("If it keeps waiting"), "{entered}");
        assert!(entered.contains("the wait ends"), "{entered}");

        // Wait / start / wait: the second entry shows what is left, and
        // exhaustion wording follows from the budget.
        turn.observe(&AgentEvent::ExecutionStarted);
        turn.update_job_wait(t0 + StdDuration::from_secs(4), limit);
        turn.observe(&clean_end());
        let JobWaitChange::Entered { remaining } =
            turn.update_job_wait(t0 + StdDuration::from_secs(5), limit)
        else {
            panic!("a second wait begins");
        };
        let second = job_wait_entered_message(&turn, limit, remaining, now);
        assert!(second.contains("6s of it left"), "{second}");
        let during = job_wait_exhausted_message(&turn, limit, false);
        assert!(during.contains("was exhausted while it waited"), "{during}");
        assert!(during.contains("job list unavailable"), "{during}");
        let on_entry = job_wait_exhausted_message(&turn, limit, true);
        assert!(
            on_entry.contains("had already been used up by its earlier waits"),
            "{on_entry}"
        );

        // Start / settle: the cleared level says what the CLI reported, not
        // that no job exists.
        turn.observe(&AgentEvent::ExecutionStarted);
        turn.observe(&omp_level(false));
        assert!(!turn.background_pending);
        let settled = omp_level(false).payload();
        assert_eq!(
            settled["message"],
            "no pending background work reported by the CLI"
        );
        assert_eq!(settled["pending"], false);
        assert!(
            !settled["message"]
                .as_str()
                .unwrap()
                .contains("no background jobs"),
            "{settled}"
        );

        // Named Claude jobs keep their descriptions and count.
        let mut named = SingleShotTurn::default();
        named.observe(&named_level(true));
        named.observe(&clean_end());
        let message = job_wait_entered_message(&named, limit, limit, now);
        assert!(
            message.contains("1 background job(s) running: d"),
            "{message}"
        );
        assert_eq!(
            named_level(true).payload()["message"],
            "1 background job(s) running: d"
        );
    }

    /// The start note says only that main-agent activity was observed.
    #[test]
    fn the_execution_start_note_claims_only_observed_activity() {
        let payload = AgentEvent::ExecutionStarted.payload();
        assert_eq!(payload["kind"], "execution_started");
        assert_eq!(payload["message"], "main-agent activity observed");
        assert_eq!(
            AgentEvent::ExecutionStarted.event_type(),
            EventType::SessionNote
        );
    }

    /// A manager running a real `OmpAdapter` against the scripted fixture.
    async fn start_omp_single_shot(
        state_dir: &std::path::Path,
        script: &crate::test_support::OmpScript,
        timers: TurnTimers,
        idle: chrono::Duration,
    ) -> (SqlitePool, String, Arc<SessionManager>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
        let adapter: Arc<dyn AgentAdapter> = Arc::new(crate::adapter::OmpAdapter::with_binary(
            &script.binary,
            state_dir,
        ));
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            idle,
            Arc::new(Notify::new()),
            timers,
        );
        let role = RoleConfig {
            model: Some("openai-codex/gpt-5.6-terra".to_string()),
            sandboxed: true,
            isolation: crate::adapter::Isolation::default(),
            ..single_shot_role_config()
        };
        manager
            .start(&session_id, "omp", "go", &role, SessionKind::SingleShot)
            .await
            .unwrap();
        (pool, session_id, manager)
    }

    fn omp_wake_steps(awaiting: serde_json::Value, finish: bool) -> serde_json::Value {
        let mut steps = vec![
            json!({"op": "agent_start"}),
            json!({"op": "assistant", "text": "started the service"}),
            json!({"op": "agent_end", "terminal": false, "awaiting": awaiting}),
            json!({"op": "prompt_result", "settled": false}),
            json!({"op": "gate", "name": "wake"}),
        ];
        if finish {
            steps.extend([
                json!({"op": "agent_start"}),
                json!({"op": "report", "args": {"outcome": "done", "summary": "s"}}),
                json!({"op": "assistant", "text": "finished"}),
                json!({"op": "agent_end"}),
                json!({"op": "session_settled"}),
            ]);
        }
        serde_json::Value::Array(steps)
    }

    /// Through the real session and adapter: pending work with no identities
    /// is neither nudged nor reaped while it is pending, and a wake that
    /// reports and ends completes the turn once.
    #[tokio::test]
    async fn omp_pending_work_is_not_nudged_or_reaped_and_a_wake_completes() {
        let dir = TempDir::new();
        let script =
            crate::test_support::OmpScript::new(&dir.0, omp_wake_steps(json!(true), true), &[]);
        let (pool, session_id, manager) =
            start_omp_single_shot(&dir.0, &script, fast_timers(3), chrono::Duration::zero()).await;
        wait_for_note(&pool, &session_id, "job_wait").await;

        // The reaper's staleness is zero, so freshness cannot be what saves
        // the turn; and the nudge timer (150 ms) gets several chances.
        reap_once(&manager).await;
        tokio::time::sleep(StdDuration::from_millis(700)).await;
        reap_once(&manager).await;
        let kinds = session_notes(&pool, &session_id).await;
        assert!(
            !kinds
                .iter()
                .any(|k| ["nudge", "no_report", "lingered"].contains(&k.as_str())),
            "{kinds:?}"
        );
        assert_eq!(
            sessions::get(&pool, &session_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Active
        );

        script.release("wake");
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle, "{:?}", script.lines());
        assert_eq!(run.end_reason, None);
        assert_eq!(script.prompts_received(), 1, "no nudge or second prompt");
        let kinds = session_notes(&pool, &session_id).await;
        assert!(!kinds.iter().any(|k| k == "nudge"), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| *k == "execution_started").count(),
            2,
            "{kinds:?}"
        );
    }

    /// The same pending work, never woken: bounded by the existing wait
    /// budget, closing the turn as no-report.
    #[tokio::test]
    async fn omp_pending_work_that_never_wakes_closes_at_the_budget() {
        let dir = TempDir::new();
        let script =
            crate::test_support::OmpScript::new(&dir.0, omp_wake_steps(json!(true), false), &[]);
        let timers = TurnTimers {
            job_wait_limit: StdDuration::from_millis(400),
            ..fast_timers(3)
        };
        let (pool, session_id, _manager) =
            start_omp_single_shot(&dir.0, &script, timers, chrono::Duration::hours(1)).await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let kinds = session_notes(&pool, &session_id).await;
        assert!(kinds.iter().any(|k| k == "job_wait"), "{kinds:?}");
        assert!(kinds.iter().any(|k| k == "no_report"), "{kinds:?}");
        assert!(!kinds.iter().any(|k| k == "nudge"), "{kinds:?}");
    }

    /// Settled with nothing signalled pending (a named-service shape): the
    /// ordinary silence handling applies, the budget does not.
    #[tokio::test]
    async fn omp_work_not_signalled_pending_is_nudged_like_any_silence() {
        let dir = TempDir::new();
        let script = crate::test_support::OmpScript::new(
            &dir.0,
            omp_wake_steps(json!("ABSENT"), false),
            &[],
        );
        let (pool, session_id, _manager) =
            start_omp_single_shot(&dir.0, &script, fast_timers(1), chrono::Duration::hours(1))
                .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let kinds = session_notes(&pool, &session_id).await;
        assert!(kinds.iter().any(|k| k == "nudge"), "{kinds:?}");
        assert!(!kinds.iter().any(|k| k == "job_wait"), "{kinds:?}");
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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

    // ---- malformed CLI output as evidence (step 2a of 7) ----

    const REPORT_CALL_ID: &str = "toolu_01A1jR6fBWvWfbjeVUvegCuS";

    /// A production-shaped `report_outcome` call line.
    fn raw_report_call() -> serde_json::Value {
        json!({"op": "raw", "line": {
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": REPORT_CALL_ID,
                "name": chocofactory_core::mcp::qualified_report_outcome_tool_name(),
                "input": {"outcome": "done", "summary": ""},
            }]},
            "session_id": "s",
        }})
    }

    /// The result the real CLI sends for that call: no `is_error`, array
    /// content. `with_flag` adds an `is_error` of the given value.
    fn raw_report_result(with_flag: Option<serde_json::Value>) -> serde_json::Value {
        let mut block = json!({
            "tool_use_id": REPORT_CALL_ID, "type": "tool_result",
            "content": [{"type": "text", "text": "Accepted outcome 'done'."}],
        });
        if let Some(flag) = with_flag {
            block["is_error"] = flag;
        }
        json!({"op": "raw", "line": {
            "type": "user", "message": {"role": "user", "content": [block]}, "session_id": "s",
        }})
    }

    fn raw_result_line(with: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let mut line = json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "",
            "duration_ms": 1200, "num_turns": 2, "total_cost_usd": 0.01,
            "usage": {"input_tokens": 3, "output_tokens": 9},
            "session_id": "s",
        });
        for (key, value) in with {
            line[*key] = value.clone();
        }
        json!({"op": "raw", "line": line})
    }

    fn without_key(mut step: serde_json::Value, key: &str) -> serde_json::Value {
        step["line"].as_object_mut().unwrap().remove(key);
        step
    }

    fn protocol_errors(stored: &[chocofactory_core::models::Event]) -> Vec<String> {
        stored
            .iter()
            .filter(|e| e.payload["protocol_error"] == true)
            .map(|e| e.payload["message"].as_str().unwrap().to_string())
            .collect()
    }

    /// NA8(a): the control. A production-shaped report whose success carries
    /// no `is_error` still completes the turn.
    #[tokio::test]
    async fn na8a_a_report_success_without_is_error_completes_the_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[]),
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(0)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
        assert_eq!(run.end_reason, None);
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        assert_eq!(protocol_errors(&stored), Vec::<String>::new());
        let results: Vec<_> = stored
            .iter()
            .filter(|e| e.event_type == EventType::ToolResult)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload["is_error"], false);
        assert_eq!(
            results[0].payload["tool"],
            chocofactory_core::mcp::qualified_report_outcome_tool_name()
        );
    }

    /// NA8(b): a report whose result carries `is_error: null` is not a
    /// successful report, so a clean end after it is a turn that never
    /// reported.
    #[tokio::test]
    async fn na8b_a_report_result_with_a_null_flag_is_not_a_report() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(Some(json!(null))),
                raw_result_line(&[]),
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(0)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited, "never Idle: {run:?}");
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        assert_eq!(
            protocol_errors(&stored),
            vec!["malformed CLI output: a tool_result block's is_error is null, not true or false"]
        );
        assert!(
            !stored.iter().any(|e| e.event_type == EventType::ToolResult),
            "no successful ToolResult for the call: {stored:?}"
        );
    }

    /// NA8(c): a `result` line with no `is_error` is not a clean end.
    #[tokio::test]
    async fn na8c_a_result_line_without_its_flag_ends_the_turn_as_an_error() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(None),
                without_key(raw_result_line(&[]), "is_error"),
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(0)
        };
        let (pool, session_id, _manager) = start_single_shot(binary, timers).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited, "never Idle: {run:?}");
        assert_eq!(run.end_reason, None);
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let tail: Vec<_> = stored
            .iter()
            .rev()
            .take(2)
            .map(|e| (e.event_type, e.payload.clone()))
            .collect();
        assert_eq!(
            tail,
            vec![
                (EventType::TurnCompleted, json!({"is_error": true})),
                (
                    EventType::Error,
                    json!({"message": "malformed CLI output: a result line's is_error is missing, not true or false",
                           "protocol_error": true})
                ),
            ]
        );
        let usage: Vec<(Option<f64>, Option<i64>)> =
            sqlx::query_as("SELECT cost_usd, output_tokens FROM turn_usage WHERE session_id = ?")
                .bind(&session_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            usage,
            vec![(Some(0.01), Some(9))],
            "the usage the line carried is kept"
        );
    }

    fn load_timers() -> TurnTimers {
        TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(0)
        }
    }

    /// A numeric-parent `result` whose owner cannot be known. Dropping it
    /// without an end would let a failed attempt be followed by a valid
    /// report and a clean end and count as a success, which attributing the
    /// line to the main agent would not: so the loss is an error end.
    #[tokio::test]
    async fn a_failed_end_with_a_malformed_parent_is_not_followed_by_a_success() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_result_line(&[("parent_tool_use_id", json!(7)), ("is_error", json!(true))]),
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[]),
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, load_timers()).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited, "never Idle: {run:?}");
        assert_eq!(run.end_reason, None);
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        assert_eq!(
            protocol_errors(&stored),
            vec![
                "malformed CLI output: parent_tool_use_id is a number, not a non-empty string or null"
            ]
        );
        let ends: Vec<_> = stored
            .iter()
            .filter(|e| e.event_type == EventType::TurnCompleted)
            .map(|e| e.payload.clone())
            .collect();
        assert_eq!(
            ends.first(),
            Some(&json!({"is_error": true})),
            "the refusal is the turn's first end: {ends:?}"
        );
    }

    /// Control: a clean (`is_error: false`) malformed-parent `result` also
    /// ends an unfinished turn as an error, even after a valid report.
    #[tokio::test]
    async fn a_clean_end_with_a_malformed_parent_ends_an_unfinished_turn_as_an_error() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[("parent_tool_use_id", json!(7))]),
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, load_timers()).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited, "never Idle: {run:?}");
        assert_eq!(run.end_reason, None);
    }

    /// Control: a helper's `result` with a valid parent is scoped to the
    /// helper and does not end the main turn.
    #[tokio::test]
    async fn a_helper_scoped_end_does_not_end_the_main_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_result_line(&[
                    ("parent_tool_use_id", json!("toolu_01W3VQxTmyXy3B7b1bmz9YaX")),
                    ("is_error", json!(true)),
                ]),
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[]),
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, load_timers()).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
        assert_eq!(run.end_reason, None);
    }

    /// Control: a turn that has already completed stays `Idle` when an
    /// unattributable end arrives after it. The late end is history.
    #[tokio::test]
    async fn a_malformed_parent_end_after_completion_does_not_reclassify_the_turn() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[]),
                raw_result_line(&[("parent_tool_use_id", json!(7)), ("is_error", json!(true))]),
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, load_timers()).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
        assert_eq!(run.end_reason, None);
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let late: Vec<_> = stored
            .iter()
            .filter(|e| e.event_type == EventType::TurnCompleted)
            .map(|e| e.payload.clone())
            .collect();
        assert_eq!(
            late,
            vec![
                json!({"is_error": false}),
                json!({"is_error": true, "after_completion": true}),
            ]
        );
    }

    /// A main `result` with no flag fails an unfinished turn (NA8(c)) but
    /// does not reopen or reclassify one that has already completed.
    #[tokio::test]
    async fn a_missing_flag_end_after_completion_leaves_the_turn_idle() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                raw_report_call(),
                raw_report_result(None),
                raw_result_line(&[]),
                without_key(raw_result_line(&[]), "is_error"),
            ]),
        );
        let (pool, session_id, _manager) = start_single_shot(binary, load_timers()).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
        assert_eq!(run.end_reason, None);
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let late = stored
            .iter()
            .filter(|e| e.payload["after_completion"] == true)
            .count();
        assert_eq!(late, 2, "the diagnostic and the end are both history");
    }

    /// The fixture's report results are the shapes the real CLI sends: a
    /// success has no `is_error` key and array content naming the outcome; a
    /// rejection is `is_error: true` with string content. The call's tool
    /// name is the one Rust builds, not a copy of it.
    #[test]
    fn the_script_fixtures_report_results_have_the_real_cli_shapes() {
        let dir = TempDir::new();
        let script = dir.0.join("script.json");
        std::fs::write(
            &script,
            json!([
                {"op": "report", "outcome": "done"},
                {"op": "report", "outcome": "lgtm", "is_error": true},
            ])
            .to_string(),
        )
        .unwrap();
        let output = std::process::Command::new(fixture_binary("fake_claude_script.py"))
            .env("FAKE_CLAUDE_SCRIPT", &script)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let blocks = |kind: &str| -> Vec<serde_json::Value> {
            lines
                .iter()
                .flat_map(|l| {
                    l["message"]["content"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter(|b| b["type"] == kind)
                .collect()
        };
        let calls = blocks("tool_use");
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(
                call["name"],
                chocofactory_core::mcp::qualified_report_outcome_tool_name()
            );
        }
        let results = blocks("tool_result");
        assert_eq!(results.len(), 2);
        let success = results[0].as_object().unwrap();
        assert!(!success.contains_key("is_error"), "{success:?}");
        assert_eq!(
            results[0]["content"],
            json!([{"type": "text", "text": "Accepted outcome 'done'."}])
        );
        assert_eq!(results[1]["is_error"], true);
        assert!(results[1]["content"].is_string());
    }

    /// Neither a top-level nor a wrapped `ProtocolError` is main-agent
    /// activity, and neither changes what the turn is waiting for.
    #[test]
    fn a_protocol_error_changes_no_turn_state() {
        let diagnostic = AgentEvent::ProtocolError {
            message: "malformed CLI output: a result line's is_error is null, not true or false"
                .to_string(),
        };
        for event in [
            diagnostic.clone(),
            AgentEvent::Subagent {
                parent_tool_use_id: "toolu_01W3VQxTmyXy3B7b1bmz9YaX".to_string(),
                event: Box::new(diagnostic),
            },
        ] {
            let mut turn = SingleShotTurn {
                waiting_for_report: true,
                ..Default::default()
            };
            turn.pending_reports.insert("toolu_01pending".to_string());
            assert!(matches!(turn.observe(&event), TurnStep::Continue));
            assert!(
                turn.waiting_for_report,
                "a diagnostic is not the agent working again"
            );
            assert_eq!(
                turn.pending_reports,
                std::collections::HashSet::from(["toolu_01pending".to_string()])
            );
            assert!(!turn.reported);
            assert!(!turn.completed);
            assert!(!turn.errored);
            assert!(!turn.gave_up);
        }
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
    /// grace period runs out and the cleanup is verified, so the completed
    /// turn stays complete; the `lingered` note records the kill, and what the
    /// turn emitted after completing is flagged on the timeline instead of
    /// blending in.
    #[tokio::test]
    async fn a_process_that_outlives_its_completed_turn_is_killed_and_the_turn_stays_complete() {
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
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        assert_eq!(run.cleanup, Some(SessionCleanup::Verified));
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
        assert_eq!(run.cleanup, Some(SessionCleanup::Verified));
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
            role: "coder".to_string(),
            project_id: None,
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
        cleanup: Option<SessionCleanup>,
        expected: (SessionStatus, Option<SessionEndReason>),
    }

    fn turn_with(f: impl FnOnce(&mut SingleShotTurn)) -> SingleShotTurn {
        let mut turn = SingleShotTurn::default();
        f(&mut turn);
        turn
    }

    /// A pause `--now` (#295) ranks below a cancel and above the daemon
    /// stopping; a completed turn keeps its ordinary result; a standing
    /// session is never ended `paused`.
    #[test]
    fn a_pause_ranks_between_cancelled_and_daemon_stopped() {
        use SessionEndReason::{Cancelled, DaemonStopped, Paused};
        use SessionKind::{SingleShot, Standing};
        use SessionStatus::{Exited, Idle};
        let silent = SingleShotTurn::default();
        let completed = turn_with(|t| t.completed = true);
        let ranked = |kind, turn: &SingleShotTurn, cancelled, paused, stopping| {
            final_run_state_ranked(kind, turn, false, false, cancelled, paused, stopping, None)
        };
        assert_eq!(
            ranked(SingleShot, &silent, false, true, false),
            (Exited, Some(Paused))
        );
        assert_eq!(
            ranked(SingleShot, &silent, true, true, false),
            (Exited, Some(Cancelled)),
            "cancelled beats paused"
        );
        assert_eq!(
            ranked(SingleShot, &silent, false, true, true),
            (Exited, Some(Paused)),
            "paused beats daemon stopped"
        );
        assert_eq!(
            ranked(SingleShot, &silent, false, false, true),
            (Exited, Some(DaemonStopped))
        );
        assert_eq!(
            ranked(SingleShot, &completed, false, true, false),
            (Idle, None),
            "a completed turn keeps its ordinary result"
        );
        assert_eq!(
            ranked(Standing, &silent, false, true, false).1,
            None,
            "a pause never stops a standing session"
        );
    }

    /// #346: a run ended because Claude Code did not apply the role's
    /// settings ranks after the requested ends (cancel, pause, daemon stop)
    /// and ahead of every observed one, for a standing session too.
    #[test]
    fn a_settings_end_ranks_after_requested_ends_and_ahead_of_observed_ones() {
        use SessionEndReason::{
            Cancelled, ClaudeSettingsRejected, ClaudeSettingsUnconfirmed, DaemonStopped, Paused,
        };
        use SessionKind::{SingleShot, Standing};
        use SessionStatus::Exited;
        let rejected = turn_with(|t| t.settings_end = Some(ClaudeSettingsRejected));
        let unconfirmed = turn_with(|t| t.settings_end = Some(ClaudeSettingsUnconfirmed));
        // Observed ends that would otherwise win: a gave-up turn, a reaped one,
        // a clean exit with no report.
        let rejected_and_gave_up = turn_with(|t| {
            t.settings_end = Some(ClaudeSettingsRejected);
            t.gave_up = true;
        });
        let ranked = |kind, turn: &SingleShotTurn, clean, reaped, cancelled, paused, stopping| {
            final_run_state_ranked(kind, turn, clean, reaped, cancelled, paused, stopping, None)
        };
        for kind in [SingleShot, Standing] {
            assert_eq!(
                ranked(kind, &rejected, false, false, false, false, false),
                (Exited, Some(ClaudeSettingsRejected)),
                "{kind:?}"
            );
            assert_eq!(
                ranked(kind, &unconfirmed, false, false, false, false, false),
                (Exited, Some(ClaudeSettingsUnconfirmed)),
                "{kind:?}"
            );
            assert_eq!(
                ranked(kind, &rejected, false, false, true, false, false).1,
                Some(Cancelled),
                "cancel beats it: {kind:?}"
            );
            assert_eq!(
                ranked(kind, &rejected, false, false, false, false, true).1,
                Some(DaemonStopped),
                "daemon stop beats it: {kind:?}"
            );
        }
        assert_eq!(
            ranked(SingleShot, &rejected, false, false, false, true, false).1,
            Some(Paused),
            "pause beats it"
        );
        assert_eq!(
            ranked(
                SingleShot,
                &rejected_and_gave_up,
                true,
                true,
                false,
                false,
                false
            ),
            (Exited, Some(ClaudeSettingsRejected)),
            "it beats gave_up and reaped"
        );
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                cleanup: None,
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
                    case.stopping,
                    case.cleanup
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
        sessions::update_status(pool, &session_id, SessionStatus::Idle, None, None, None)
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
            role: "coder".to_string(),
            project_id: Some("P".to_string()),
            isolation: crate::adapter::Isolation::Isolated {
                skills: Vec::new(),
                memory: true,
            },
            ..role_config()
        }
    }

    #[tokio::test]
    async fn chat_resume_refuses_a_bad_claude_level_before_reserving_anything() {
        let pool = connect_in_memory().await.unwrap();
        let manager = SessionManager::new(
            pool.clone(),
            Registry::new(vec![Arc::new(crate::adapter::ClaudeAdapter::with_binary(
                "/nonexistent/claude",
            ))]),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );
        let session_id = idle_chat_session_on(&pool, "claude").await;
        let bad = RoleConfig {
            model: Some("claude-sonnet-5-5:auto".to_string()),
            ..role_config()
        };
        let result = manager.send_message(&session_id, "hi", &bad).await;
        match result {
            Err(SessionError::RoleRejected(message)) => {
                assert!(message.contains(":auto"), "{message}");
                assert!(message.contains("`chat`"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        let row = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Idle);
        // A valid level gets past the check (the missing binary fails later).
        let good = RoleConfig {
            model: Some("claude-sonnet-5-5:high".to_string()),
            ..role_config()
        };
        let result = manager.send_message(&session_id, "hi", &good).await;
        assert!(
            !matches!(result, Err(SessionError::RoleRejected(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn chat_resume_on_omp_with_memory_is_accepted_like_a_claude_one() {
        let dir = std::env::temp_dir().join(format!("choco-omp-chat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("omp-was-run");
        let wrapper = dir.join("omp-wrapper");
        crate::test_support::write_executable(
            &wrapper,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        );
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
        let result = manager
            .send_message(&session_id, "hi", &memory_role_config())
            .await;
        assert!(
            !matches!(result, Err(SessionError::RoleRejected(_))),
            "{result:?}"
        );
        for _ in 0..200 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(marker.exists(), "omp must have been started");
        assert!(dir.join("state").join("memory").join("P").is_dir());
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
            role: "coder".to_string(),
            project_id: None,
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
        let (pool, session_id, _job) =
            assert_escaped_job_is_killed_with_pid(extra_steps, timers, action).await;
        (pool, session_id)
    }

    /// [`assert_escaped_job_is_killed`], also returning the job's pid.
    async fn assert_escaped_job_is_killed_with_pid(
        extra_steps: Vec<serde_json::Value>,
        timers: TurnTimers,
        action: EndAction,
    ) -> (SqlitePool, String, u32) {
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
        let run = wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        let note = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(note.contains(&format!("{job} (")), "{note}");
        // The job was signalled and the last check no longer saw it, so the
        // cleanup is verified whichever way the turn ended.
        assert_eq!(run.cleanup, Some(SessionCleanup::Verified));
        (pool, session_id, job)
    }

    #[tokio::test]
    async fn an_escaped_job_is_killed_when_the_cli_exits_on_its_own_after_reporting() {
        let (pool, id, job) = assert_escaped_job_is_killed_with_pid(
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
        assert_eq!(run.cleanup, Some(SessionCleanup::Verified));
        // An independent look at the same fact: the process list, not the
        // `kill(pid, 0)` probe the helper used, shows the job gone (or only a
        // zombie awaiting its reaper, which is not alive).
        let ps_stat = |pid: u32| {
            std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .output()
                .expect("ps runs")
        };
        // The invocation itself is sound: it finds this very process.
        let own = ps_stat(std::process::id());
        assert!(
            own.status.success() && !own.stdout.is_empty(),
            "ps cannot look up a live pid: status {:?}, stderr {:?}",
            own.status.code(),
            String::from_utf8_lossy(&own.stderr)
        );
        let listed = ps_stat(job);
        let stat = String::from_utf8_lossy(&listed.stdout).trim().to_string();
        let diagnostics = String::from_utf8_lossy(&listed.stderr).trim().to_string();
        eprintln!(
            "ps -p {job} -o stat= -> status {:?}, stdout {stat:?}, stderr {diagnostics:?}",
            listed.status.code()
        );
        // A pid ps does not list is "no match": exit 1 with nothing printed.
        // Any other failure, or a diagnostic, is not an observation.
        assert!(
            diagnostics.is_empty(),
            "ps reported a problem: {diagnostics:?}"
        );
        assert!(
            (listed.status.code() == Some(1) && stat.is_empty())
                || (listed.status.success() && stat.starts_with('Z')),
            "ps still lists the job as alive, or failed: status {:?}, stdout {stat:?}",
            listed.status.code()
        );
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
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        assert_eq!(run.cleanup, Some(SessionCleanup::Verified));
    }

    /// The CLI's own limit message, with the machine-readable markers.
    fn raw_limit_message() -> serde_json::Value {
        json!({"op": "raw", "line": {
            "type": "assistant",
            "message": {
                "model": "<synthetic>",
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "You've hit your session limit \u{b7} resets 3:40pm (Europe/Berlin)"
                }],
            },
            "error": "rate_limit",
            "isApiErrorMessage": true,
            "apiErrorStatus": 429,
            "session_id": "limit-session",
        }})
    }

    fn raw_tool_call() -> serde_json::Value {
        json!({"op": "raw", "line": {
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": "toolu_ls", "name": "Bash", "input": {"command": "ls"}
            }]},
            "session_id": "limit-session",
        }})
    }

    fn raw_tool_result() -> serde_json::Value {
        json!({"op": "raw", "line": {
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": "toolu_ls", "content": "ok", "is_error": false
            }]},
            "session_id": "limit-session",
        }})
    }

    /// Runs a fixture with an owned escaped job to the end of its turn and
    /// returns the session's final row.
    async fn run_with_escaped_job(
        extra_steps: Vec<serde_json::Value>,
    ) -> chocofactory_core::models::Session {
        let (pool, id, _job) =
            assert_escaped_job_is_killed_with_pid(extra_steps, fast_timers(3), EndAction::Nothing)
                .await;
        sessions::get(&pool, &id).await.unwrap().unwrap()
    }

    /// The main agent hit the limit and was cut off with nothing after it:
    /// with its leftover verified gone the run is the interruption it would
    /// have been without one, not a success.
    #[tokio::test]
    async fn a_terminating_cutoff_with_verified_cleanup_is_interrupted_not_lingered() {
        let run = run_with_escaped_job(vec![
            json!({"op": "report", "outcome": "done"}),
            json!({"op": "usage_limit"}),
            json!({"op": "sleep", "seconds": 60}),
        ])
        .await;
        assert_eq!(
            (run.status, run.end_reason, run.cleanup),
            (
                SessionStatus::Exited,
                Some(SessionEndReason::Interrupted),
                Some(SessionCleanup::Verified)
            )
        );
    }

    /// A limit the agent carried on past does not make a later, ordinary
    /// failure resumable, whatever kind of work it carried on with and
    /// whether or not it ended a turn cleanly in between.
    #[tokio::test]
    async fn a_recovered_limit_does_not_gain_linger_resume() {
        let generic_error = json!({"op": "result", "is_error": true});
        let cases: Vec<(&str, Vec<serde_json::Value>)> = vec![
            (
                "text after the limit",
                vec![
                    raw_limit_message(),
                    json!({"op": "text", "text": "carrying on"}),
                    generic_error.clone(),
                ],
            ),
            (
                "tool call and result after the limit",
                vec![
                    raw_limit_message(),
                    raw_tool_call(),
                    raw_tool_result(),
                    generic_error.clone(),
                ],
            ),
            (
                "a clean report-less end after the limit",
                vec![
                    raw_limit_message(),
                    json!({"op": "result"}),
                    generic_error.clone(),
                ],
            ),
        ];
        for (name, mut steps) in cases {
            steps.push(json!({"op": "sleep", "seconds": 60}));
            let run = run_with_escaped_job(steps).await;
            assert_eq!(
                (run.status, run.end_reason, run.cleanup),
                (
                    SessionStatus::Exited,
                    Some(SessionEndReason::Lingered),
                    Some(SessionCleanup::Verified)
                ),
                "{name}"
            );
        }
    }

    /// Control for the case above: what a sub-agent does after the limit is
    /// not the main agent working, so the cutoff stands. The ending is a
    /// generic error, not a second limit, which would hide a mistake.
    #[tokio::test]
    async fn work_by_a_helper_after_the_limit_does_not_undo_the_cutoff() {
        let run = run_with_escaped_job(vec![
            raw_limit_message(),
            json!({"op": "text", "text": "helper busy", "parent": "toolu_01W3VQxTmyXy3B7b1bmz9YaX"}),
            json!({"op": "result", "is_error": true}),
            json!({"op": "sleep", "seconds": 60}),
        ])
        .await;
        assert_eq!(
            (run.status, run.end_reason, run.cleanup),
            (
                SessionStatus::Exited,
                Some(SessionEndReason::Interrupted),
                Some(SessionCleanup::Verified)
            )
        );
    }

    /// An accepted report that never got a clean end is not a completed
    /// turn, however well the cleanup went: an error end stays `Lingered`, and
    /// an exit with no end at all is the plain `NoReport` / crash it was.
    #[tokio::test]
    async fn a_report_without_a_clean_end_cannot_gain_cleanup_success() {
        let run = run_with_escaped_job(vec![
            json!({"op": "report", "outcome": "done"}),
            json!({"op": "result", "is_error": true}),
            json!({"op": "sleep", "seconds": 60}),
        ])
        .await;
        assert_eq!(
            (run.status, run.end_reason, run.cleanup),
            (
                SessionStatus::Exited,
                Some(SessionEndReason::Lingered),
                Some(SessionCleanup::Verified)
            )
        );
        // Without an end the process simply exits: exit 0 never reported a
        // completed turn, exit 17 is a crash.
        for (code, expected) in [(0, Some(SessionEndReason::NoReport)), (17, None)] {
            let dir = TempDir::new();
            let binary = script_binary(
                &dir.0,
                json!([
                    {"op": "read_turn"},
                    {"op": "report", "outcome": "done"},
                    {"op": "exit", "code": code},
                ]),
            );
            let (pool, session_id, _manager) =
                start_leftovers(binary, fast_timers(3), chrono::Duration::hours(1), None).await;
            let run = wait_until_final(&pool, &session_id).await;
            assert_eq!(
                (run.status, run.end_reason),
                (SessionStatus::Exited, expected),
                "exit {code}"
            );
        }
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

    /// With the table unreadable, the group kill is still sent, the turn stays
    /// `Lingered` instead of completing the way it does with a working reader,
    /// and the operator is told.
    #[tokio::test]
    async fn an_unreadable_process_table_is_reported_and_leaves_the_turn_lingering() {
        async fn run(
            reader: Option<crate::proc_table::ProcReader>,
        ) -> (
            SessionStatus,
            Option<SessionEndReason>,
            Option<SessionCleanup>,
            Vec<String>,
        ) {
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
                run.cleanup,
                session_notes(&pool, &session_id).await,
            )
        }
        let broken: crate::proc_table::ProcReader =
            Arc::new(|_| Err(std::io::Error::other("injected failure")));
        let (status, reason, cleanup, notes) = run(Some(broken)).await;
        let (good_status, good_reason, good_cleanup, good_notes) = run(None).await;
        // Nothing could confirm the unreadable run, so the grace kill still
        // leaves it lingering; the same turn with a working reader is verified
        // and stays complete.
        assert_eq!(
            (status, reason),
            (SessionStatus::Exited, Some(SessionEndReason::Lingered))
        );
        assert_eq!(cleanup, Some(SessionCleanup::Unverified));
        assert_eq!((good_status, good_reason), (SessionStatus::Idle, None));
        assert_eq!(good_cleanup, Some(SessionCleanup::Verified));
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

    /// `Lingered` records that choco tried to clean up after the turn, not
    /// that anything was seen running. A turn that ends in an error with no
    /// report, a pre-kill read that fails and a sweep that finds nothing still
    /// ends `Lingered`: the note says the check could not be made, there is no
    /// survivor and no killed process, and the unreadable table is flagged.
    #[tokio::test]
    async fn a_lingered_run_need_not_have_been_seen_running() {
        let dir = TempDir::new();
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "result", "is_error": true},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        // Marker-less scans are the pre-kill and turn-event reads; sweep scans
        // carry a marker.
        let reader: crate::proc_table::ProcReader = Arc::new(|marker| match marker {
            None => Err(std::io::Error::other("injected pre-kill failure")),
            Some(_) => Ok(Vec::new()),
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(
            (run.status, run.end_reason),
            (SessionStatus::Exited, Some(SessionEndReason::Lingered))
        );
        assert_eq!(
            note_message(&pool, &session_id, "lingered")
                .await
                .as_deref(),
            Some(
                "0.4s after its turn ended, could not check whether the agent's process \
                 group was still running; sent SIGKILL to the group"
            )
        );
        let notes = session_notes(&pool, &session_id).await;
        assert!(
            notes.contains(&"leftovers_unchecked".to_string()),
            "{notes:?}"
        );
        assert!(
            !notes.contains(&"leftovers_killed".to_string()),
            "nothing was seen running, so no process is named: {notes:?}"
        );
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
        let run = wait_until_final(&pool, &session_id).await;
        // The survivor changes neither how the turn ended nor its status.
        assert_eq!(run.end_reason, None);
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.cleanup, Some(SessionCleanup::Unverified));
        let message = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(
            message.contains("still running at the last check"),
            "{message}"
        );
        assert!(message.contains(&format!("{GHOST} (ghost)")), "{message}");
        // Listed once, though several sites swept.
        assert_eq!(message.matches("(ghost)").count(), 1, "{message}");
    }

    /// A last check that sees a real job killed and gone, but also one
    /// process still alive, is not verified: the survivor alone decides.
    #[tokio::test]
    async fn a_mixed_last_check_keeps_cleanup_unverified() {
        const GHOST: i32 = 99_999_999;
        let dir = TempDir::new();
        let job_file = dir.0.join("job.pid");
        let _job_guard = crate::test_support::KillPidFileOnDrop(job_file.clone());
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                escaped_job(&job_file),
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        // SAFETY: only reads the process's own uid.
        let uid = unsafe { libc::geteuid() };
        // Every read is the real table plus one entry nothing can kill.
        let reader: crate::proc_table::ProcReader = Arc::new(move |marker| {
            let mut table = crate::proc_table::read(marker)?;
            table.push(crate::proc_table::ProcEntry {
                pid: GHOST,
                ppid: 1,
                pgid: GHOST,
                sid: GHOST,
                uid,
                start: 1,
                zombie: false,
                comm: "ghost".into(),
                marker: crate::proc_table::MarkerStatus::Present,
            });
            Ok(table)
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let job = read_pid_when_written(&job_file).await;
        let run = wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        let message = note_message(&pool, &session_id, "leftovers_killed")
            .await
            .expect("a leftovers_killed note");
        assert!(
            message.contains(&format!(
                "killed 1 process(es) the turn left running: {job} ("
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "1 still running at the last check: {GHOST} (ghost)"
            )),
            "{message}"
        );
        assert!(!process_alive(job));
        assert_eq!(run.cleanup, Some(SessionCleanup::Unverified));
    }

    /// One failed process-table read early in the turn keeps the cleanup
    /// unverified even though every later read, the end-of-turn sweep and the
    /// reap all succeed and nothing is seen running.
    #[tokio::test]
    async fn one_failed_process_table_read_keeps_cleanup_unverified() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let dir = TempDir::new();
        let job_file = dir.0.join("job.pid");
        let _job_guard = crate::test_support::KillPidFileOnDrop(job_file.clone());
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                escaped_job(&job_file),
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        let reads = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let (seen_reads, seen_failures) = (Arc::clone(&reads), Arc::clone(&failures));
        // The first scan of any kind (the descendant record at the turn's
        // end) fails; every later one is the real table.
        let reader: crate::proc_table::ProcReader = Arc::new(move |marker| {
            if seen_reads.fetch_add(1, SeqCst) == 0 {
                seen_failures.fetch_add(1, SeqCst);
                return Err(std::io::Error::other("injected one-off failure"));
            }
            crate::proc_table::read(marker)
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let job = read_pid_when_written(&job_file).await;
        let run = wait_until_final(&pool, &session_id).await;
        wait_until_gone(job).await;
        assert_eq!(failures.load(SeqCst), 1, "the injected failure never ran");
        assert!(reads.load(SeqCst) > 1, "no read followed the failure");
        let notes = session_notes(&pool, &session_id).await;
        assert!(
            notes.contains(&"leftovers_unchecked".to_string()),
            "{notes:?}"
        );
        if let Some(message) = note_message(&pool, &session_id, "leftovers_killed").await {
            assert!(!message.contains("still running"), "{message}");
        }
        assert_eq!(run.cleanup, Some(SessionCleanup::Unverified));
    }

    /// A refused final status write leaves the status and the cleanup as they
    /// were, together: cleanup is part of that one statement, never written
    /// ahead of it.
    #[tokio::test]
    async fn a_failed_final_status_write_leaves_cleanup_pending() {
        let dir = TempDir::new();
        let job_file = dir.0.join("job.pid");
        let _job_guard = crate::test_support::KillPidFileOnDrop(job_file.clone());
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                escaped_job(&job_file),
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
        sqlx::query(
            "CREATE TRIGGER fail_final BEFORE UPDATE OF status ON sessions \
             WHEN NEW.status <> 'active' BEGIN SELECT RAISE(ABORT, 'injected'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
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
        let job = read_pid_when_written(&job_file).await;
        // The drain task removes its map slot after the final write attempt,
        // refused or not; the status never changes, so wait on the slot.
        crate::test_support::wait_until("the drain task to finish", || async {
            if manager.sessions.lock().await.contains_key(&session_id) {
                Err("session still in the map".to_string())
            } else {
                Ok(())
            }
        })
        .await;
        wait_until_gone(job).await;
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
        assert_eq!(run.cleanup, Some(SessionCleanup::Pending));
    }

    /// The live handles of a session, once its map slot is `Live`.
    async fn live_signals(manager: &SessionManager, session_id: &str) -> SessionSignals {
        crate::test_support::wait_until("the session to be live", || async {
            match manager.sessions.lock().await.get(session_id) {
                Some(SessionSlot::Live(session)) => Ok(session.signals.clone()),
                _ => Err("not live yet".to_string()),
            }
        })
        .await
    }

    /// A turn held on a release file, then reporting, ending its turn and
    /// exiting by itself. Nothing is left behind and the output ends on its
    /// own, so only what a test changes can affect the cleanup.
    fn held_clean_turn(dir: &TempDir) -> (String, PathBuf) {
        let release = dir.0.join("release");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "wait_file", "path": release},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        (binary, release)
    }

    /// No pid at the tail means the end-of-turn sweep could not run, so
    /// nothing was checked: the cleanup is not verified even though the
    /// process exits and its output ends on their own.
    #[tokio::test]
    async fn a_missing_pid_at_the_tail_keeps_cleanup_unverified() {
        let dir = TempDir::new();
        let (binary, release) = held_clean_turn(&dir);
        let (pool, session_id, manager) = start_single_shot(
            binary,
            TurnTimers {
                grace: crate::test_support::LOAD_ALLOWANCE,
                ..fast_timers(3)
            },
        )
        .await;
        let signals = live_signals(&manager, &session_id).await;
        assert!(signals.pgid.lock().await.take().is_some(), "had a pid");
        std::fs::write(&release, "go").unwrap();
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(run.end_reason, None);
        assert_eq!(
            run.cleanup,
            Some(SessionCleanup::Unverified),
            "no sweep ran, so nothing was verified"
        );
    }

    /// A failed reap is never verified. The child is reaped behind the
    /// drain's back after the pid is retired and before it waits, so `wait`
    /// fails while the sweep, the output and the last check are all clean.
    #[tokio::test]
    async fn a_failed_reap_keeps_cleanup_unverified() {
        let dir = TempDir::new();
        let (binary, release) = held_clean_turn(&dir);
        let (pool, session_id, manager) = start_single_shot(
            binary,
            TurnTimers {
                grace: crate::test_support::LOAD_ALLOWANCE,
                ..fast_timers(3)
            },
        )
        .await;
        let signals = live_signals(&manager, &session_id).await;
        let child = signals.pgid.lock().await.expect("a live session has a pid");
        let reaped_by_test = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (seen, pgid) = (Arc::clone(&reaped_by_test), Arc::clone(&signals.pgid));
        let hook: LockHook = Box::new(move |_| {
            // Only once the tail has retired the pid (it holds this lock
            // during its own sweep, so a held lock means "not yet").
            let retired = matches!(pgid.try_lock(), Ok(guard) if guard.is_none());
            if retired && seen.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut status = 0;
                // SAFETY: waits for our own child; blocks until it exits.
                let rc = unsafe { libc::waitpid(child as i32, &mut status, 0) };
                assert_eq!(rc, child as i32, "the test could not reap the child");
            }
        });
        assert!(signals.tracker.before_lock.set(hook).is_ok());
        std::fs::write(&release, "go").unwrap();
        let run = wait_until_final(&pool, &session_id).await;
        assert!(
            reaped_by_test.load(Ordering::SeqCst) >= 1,
            "the hook never reaped the child"
        );
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(
            run.cleanup,
            Some(SessionCleanup::Unverified),
            "a failed reap was not verified"
        );
    }

    /// A job that keeps the agent's output open outlives the kill: the drain
    /// gives up on the stream after the settle time, and a stream it had to
    /// abandon is never verified, even with the process table looking empty.
    #[tokio::test]
    async fn a_stream_held_open_after_the_kill_keeps_cleanup_unverified() {
        let dir = TempDir::new();
        let holder_file = dir.0.join("holder.pid");
        let _holder_guard = crate::test_support::KillPidFileOnDrop(holder_file.clone());
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {
                    "op": "spawn_escaped", "setsid": true, "double_fork": true,
                    "keep_output": true, "pid_file": holder_file,
                },
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        // The real table, without the holder: the sweep can never reach it,
        // and no read fails.
        let hidden = holder_file.clone();
        let reader: crate::proc_table::ProcReader = Arc::new(move |marker| {
            let mut table = crate::proc_table::read(marker)?;
            if let Some(pid) = std::fs::read_to_string(&hidden)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                table.retain(|entry| entry.pid != pid);
            }
            Ok(table)
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let holder = read_pid_when_written(&holder_file).await;
        let run = wait_until_final(&pool, &session_id).await;
        // The leak itself: the holder is still running after the turn ended.
        assert!(process_alive(holder), "the holder should have survived");
        assert_eq!(run.end_reason, Some(SessionEndReason::Lingered));
        assert_eq!(run.cleanup, Some(SessionCleanup::Unverified));
    }

    /// A leftover the daemon is not permitted to kill (EPERM) is listed as a
    /// survivor, once: the failed kill lists it at once, and the final
    /// observation, which still sees it, keeps it.
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
        assert!(
            message.starts_with("1 still running at the last check"),
            "{message}"
        );
        assert_eq!(message.matches("(foreign)").count(), 1, "{message}");
    }

    /// A live agent group the daemon may not signal (EPERM) is listed as a
    /// survivor because the final observation still sees its leader alive.
    /// The failed group signal alone lists nothing (see
    /// `a_failed_group_signal_alone_lists_no_survivor`).
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

    // --- the sweep and the group kill's own members -----------------------

    /// A real `sleep 300` for the sweep-selection tests. Killed and reaped on
    /// drop, so a failed assert leaks nothing.
    struct Sleeper(Option<std::process::Child>);

    impl Sleeper {
        /// `group`: `Some(pgid)` joins that group, `None` leads its own.
        /// `own_session` calls `setsid` instead (a group leader cannot), as
        /// Claude Code's Bash tool does; `group` is then ignored.
        fn spawn(group: Option<i32>, own_session: bool) -> Self {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new("sleep");
            command.arg("300");
            if !own_session {
                command.process_group(group.unwrap_or(0));
            } else {
                // SAFETY: `setsid` is async-signal-safe.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            Sleeper(Some(command.spawn().unwrap()))
        }

        fn pid(&self) -> i32 {
            self.0.as_ref().unwrap().id() as i32
        }

        /// Not exited yet. (`kill(pid, 0)` also succeeds on an unreaped
        /// zombie, so it cannot say this.)
        fn is_running(&mut self) -> bool {
            self.0.as_mut().unwrap().try_wait().unwrap().is_none()
        }

        /// Waits for the process and returns the signal that ended it.
        fn reap_signal(&mut self) -> Option<i32> {
            use std::os::unix::process::ExitStatusExt;
            self.0.take().unwrap().wait().unwrap().signal()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// A table entry for a `sleep` of the test's own uid, started in the
    /// daemon's session unless `sid` says otherwise.
    fn sleep_entry(
        pid: i32,
        ppid: i32,
        pgid: i32,
        sid: i32,
        start: u64,
    ) -> crate::proc_table::ProcEntry {
        crate::proc_table::ProcEntry {
            pid,
            ppid,
            pgid,
            sid,
            // SAFETY: only reads the process's own uid.
            uid: unsafe { libc::geteuid() },
            start,
            zombie: false,
            comm: "sleep".into(),
            marker: crate::proc_table::MarkerStatus::Absent,
        }
    }

    /// A reader whose table depends on the scan: `None` for the scan before
    /// the kill (no marker asked for), `Some(n)` for the sweep's n-th scan.
    fn scripted_reader(
        table: impl Fn(Option<usize>) -> Vec<crate::proc_table::ProcEntry> + Send + Sync + 'static,
    ) -> crate::proc_table::ProcReader {
        let sweeps = std::sync::atomic::AtomicUsize::new(0);
        Arc::new(move |marker| {
            let call = marker.map(|_| sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
            Ok(table(call))
        })
    }

    fn sweep_tracker(reader: crate::proc_table::ProcReader) -> LeftoverTracker {
        LeftoverTracker::new(
            "CHOCOFACTORY_TURN_x".into(),
            SessionKind::SingleShot,
            reader,
        )
    }

    fn pids_of(list: &[(i32, String)]) -> Vec<i32> {
        list.iter().map(|(pid, _)| *pid).collect()
    }

    /// The agent `a`, its in-group member `p1` and, when given, `p2` in a
    /// session and group of its own; `p1` and `p2` are shown on call 1 only,
    /// or on every call when `always`.
    fn group_table(
        a: i32,
        p1: i32,
        p2: Option<i32>,
        always: bool,
    ) -> crate::proc_table::ProcReader {
        // SAFETY: only reads the process's own session id.
        let daemon_sid = unsafe { libc::getsid(0) };
        let daemon = std::process::id() as i32;
        scripted_reader(move |call| {
            let mut table = vec![sleep_entry(a, daemon, a, daemon_sid, 100)];
            if always || call.is_none_or(|n| n == 1) {
                table.push(sleep_entry(p1, a, a, daemon_sid, 200));
                if let Some(p2) = p2 {
                    table.push(sleep_entry(p2, a, p2, p2, 300));
                }
            }
            table
        })
    }

    /// T1(a): a live in-group member that the group kill already signalled is
    /// neither killed, recorded as killed, nor listed as a survivor once it is
    /// gone.
    #[tokio::test]
    async fn a_sweep_leaves_the_group_kills_member_alone() {
        let mut leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let mut p1 = Sleeper::spawn(Some(a), false);
        let tracker = sweep_tracker(group_table(a, p1.pid(), None, false));
        let returned = tracker.sweep(a, Some(a), "test").await;
        let state = tracker.state();
        assert!(
            !pids_of(&state.killed).contains(&p1.pid()),
            "P1 is not in killed: {:?}",
            state.killed
        );
        assert!(
            !pids_of(&state.survivors).contains(&p1.pid()),
            "P1 is not in survivors: {:?}",
            state.survivors
        );
        assert_eq!(returned, 0);
        assert!(p1.is_running(), "the sweep sent P1 nothing");
        assert!(leader.is_running(), "the sweep sent the agent nothing");
    }

    /// T1(b): a spared member still in the table at the last round is listed
    /// as a survivor, once, and still not as killed.
    #[tokio::test]
    async fn a_spared_member_that_stays_is_listed_as_a_survivor() {
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let mut p1 = Sleeper::spawn(Some(a), false);
        let tracker = sweep_tracker(group_table(a, p1.pid(), None, true));
        let returned = tracker.sweep(a, Some(a), "test").await;
        let state = tracker.state();
        assert_eq!(
            pids_of(&state.survivors)
                .iter()
                .filter(|p| **p == p1.pid())
                .count(),
            1,
            "P1 is in survivors exactly once: {:?}",
            state.survivors
        );
        assert!(
            !pids_of(&state.killed).contains(&p1.pid()),
            "{:?}",
            state.killed
        );
        assert_eq!(returned, 0);
        assert!(p1.is_running());
    }

    /// T1(c): mixed. The member that left the group is killed and recorded;
    /// the member still in the group is spared.
    #[tokio::test]
    async fn a_sweep_kills_the_escaped_process_and_spares_the_group_member() {
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let mut p1 = Sleeper::spawn(Some(a), false);
        let mut p2 = Sleeper::spawn(None, true);
        let tracker = sweep_tracker(group_table(a, p1.pid(), Some(p2.pid()), false));
        let returned = tracker.sweep(a, Some(a), "test").await;
        {
            let state = tracker.state();
            assert_eq!(
                pids_of(&state.killed)
                    .iter()
                    .filter(|p| **p == p2.pid())
                    .count(),
                1,
                "P2 is in killed: {:?}",
                state.killed
            );
            assert!(
                !pids_of(&state.killed).contains(&p1.pid()),
                "{:?}",
                state.killed
            );
            assert!(
                !pids_of(&state.survivors).contains(&p1.pid()),
                "{:?}",
                state.survivors
            );
        }
        assert_eq!(returned, 1);
        assert_eq!(p2.reap_signal(), Some(libc::SIGKILL));
        assert!(p1.is_running());
    }

    /// T1(d): the same table as (a) when the group kill was not delivered:
    /// the member is killed and recorded.
    #[tokio::test]
    async fn a_sweep_after_a_failed_group_kill_kills_the_member() {
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let mut p1 = Sleeper::spawn(Some(a), false);
        let tracker = sweep_tracker(group_table(a, p1.pid(), None, false));
        let returned = tracker.sweep(a, None, "test").await;
        {
            let state = tracker.state();
            assert!(
                pids_of(&state.killed).contains(&p1.pid()),
                "P1 is in killed: {:?}",
                state.killed
            );
        }
        assert_eq!(returned, 1);
        assert_eq!(p1.reap_signal(), Some(libc::SIGKILL));
    }

    /// T2: `killpg` fails (EPERM). A same-group process is then killed by pid
    /// and recorded, and the leader, still in every table, is a survivor
    /// through the final observation.
    #[tokio::test]
    async fn a_failed_group_kill_does_not_spare_the_groups_members() {
        // SAFETY: only reads the uid and probes pids with signal 0.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: running as root, every pid is signalable");
            return;
        }
        let foreign = (2..5000).find(|&pid| {
            // SAFETY: signal 0 only checks permission, and the kill is a killpg.
            let rc = unsafe { libc::killpg(pid, 0) };
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        });
        let Some(foreign) = foreign else {
            eprintln!("skipped: no foreign pid found");
            return;
        };
        let mut p3 = Sleeper::spawn(None, false);
        let p3_pid = p3.pid();
        let reader = scripted_reader(move |call| {
            let mut table = vec![sleep_entry(foreign, 1, foreign, foreign, 100)];
            if call.is_none_or(|n| n == 1) {
                table.push(sleep_entry(p3_pid, foreign, foreign, foreign, 200));
            }
            table
        });
        let tracker = sweep_tracker(reader);
        let report = kill_and_sweep(&tracker, foreign as u32, "test").await;
        {
            let state = tracker.state();
            assert!(
                pids_of(&state.killed).contains(&p3_pid),
                "P3 is recorded in killed: {:?}",
                state.killed
            );
            assert_eq!(
                pids_of(&state.survivors)
                    .iter()
                    .filter(|p| **p == foreign)
                    .count(),
                1,
                "{:?}",
                state.survivors
            );
        }
        assert_eq!(report.sweep_killed, 1);
        assert!(
            matches!(report.group, GroupKill::Failed(_)),
            "{}",
            describe_group_kill(&report.group)
        );
        assert_eq!(p3.reap_signal(), Some(libc::SIGKILL));
    }

    /// T3: the CI race. The table still shows a member alive after `killpg`,
    /// as the kernel does while its SIGKILL is pending; it is the group
    /// kill's, so it is not recorded as a leftover.
    #[tokio::test]
    async fn the_group_kills_own_member_is_not_recorded_as_a_leftover() {
        let mut leader = Sleeper::spawn(None, false);
        let l = leader.pid();
        let mut p4 = Sleeper::spawn(Some(l), false);
        let p4_pid = p4.pid();
        // SAFETY: only reads the process's own session id.
        let daemon_sid = unsafe { libc::getsid(0) };
        let daemon = std::process::id() as i32;
        let reader = scripted_reader(move |call| {
            if call.is_none_or(|n| n == 1) {
                vec![
                    sleep_entry(l, daemon, l, daemon_sid, 100),
                    sleep_entry(p4_pid, l, l, daemon_sid, 200),
                ]
            } else {
                Vec::new()
            }
        });
        let tracker = sweep_tracker(reader);
        let report = kill_and_sweep(&tracker, l as u32, "test").await;
        {
            let state = tracker.state();
            assert!(
                !pids_of(&state.killed).contains(&p4_pid),
                "the group kill's own member was recorded as a leftover (the CI flake): {:?}",
                state.killed
            );
            assert!(
                state.survivors.is_empty(),
                "no leftovers_killed note would be written: {:?}",
                state.survivors
            );
        }
        assert!(
            matches!(report.group, GroupKill::Killed(_)),
            "{}",
            describe_group_kill(&report.group)
        );
        assert_eq!(report.sweep_killed, 0);
        // The real `killpg` was delivered to both.
        assert_eq!(leader.reap_signal(), Some(libc::SIGKILL));
        assert_eq!(p4.reap_signal(), Some(libc::SIGKILL));
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

    // --- what the last check saw: records, not verdicts ---------------------

    type ScanResult = std::io::Result<Vec<crate::proc_table::ProcEntry>>;

    /// Like [`scripted_reader`], but a scan may fail.
    fn fallible_reader(
        table: impl Fn(Option<usize>) -> ScanResult + Send + Sync + 'static,
    ) -> crate::proc_table::ProcReader {
        let sweeps = std::sync::atomic::AtomicUsize::new(0);
        Arc::new(move |marker| {
            let call = marker.map(|_| sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
            table(call)
        })
    }

    /// A reader keyed by how many scans of any kind came before.
    fn counting_reader(
        table: impl Fn(usize) -> ScanResult + Send + Sync + 'static,
    ) -> crate::proc_table::ProcReader {
        let scans = std::sync::atomic::AtomicUsize::new(0);
        Arc::new(move |_| table(scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst)))
    }

    /// The phase a test is in, and how many scans it has seen in that phase.
    #[derive(Default)]
    struct Phase {
        phase: std::sync::atomic::AtomicUsize,
        scans: std::sync::atomic::AtomicUsize,
    }

    impl Phase {
        fn next(&self) {
            use std::sync::atomic::Ordering::SeqCst;
            self.phase.fetch_add(1, SeqCst);
            self.scans.store(0, SeqCst);
        }
    }

    /// A reader whose answer is a function of `(phase, scan number in phase)`.
    fn phased_reader(
        table: impl Fn(usize, usize) -> ScanResult + Send + Sync + 'static,
    ) -> (crate::proc_table::ProcReader, Arc<Phase>) {
        use std::sync::atomic::Ordering::SeqCst;
        let phase = Arc::new(Phase::default());
        let seen = Arc::clone(&phase);
        let reader: crate::proc_table::ProcReader = Arc::new(move |_| {
            let scan = seen.scans.fetch_add(1, SeqCst);
            table(seen.phase.load(SeqCst), scan)
        });
        (reader, phase)
    }

    fn marked(mut entry: crate::proc_table::ProcEntry) -> crate::proc_table::ProcEntry {
        entry.marker = crate::proc_table::MarkerStatus::Present;
        entry
    }

    fn daemon_ids() -> (i32, i32) {
        // SAFETY: only reads the process's own session id.
        (std::process::id() as i32, unsafe { libc::getsid(0) })
    }

    /// Writes the tracker's notes to a fresh session and returns the
    /// `leftovers_killed` and `leftovers_unchecked` messages.
    async fn written_notes(tracker: &LeftoverTracker) -> (Option<String>, Option<String>) {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        write_leftover_notes(&pool, &session_id, tracker, &Notify::new()).await;
        (
            note_message(&pool, &session_id, "leftovers_killed").await,
            note_message(&pool, &session_id, "leftovers_unchecked").await,
        )
    }

    fn sorted(mut pids: Vec<i32>) -> Vec<i32> {
        pids.sort_unstable();
        pids
    }

    /// Survivors and their identities cover the same pids.
    fn assert_paired(state: &TrackerState) {
        let mut listed = pids_of(&state.survivors);
        listed.sort_unstable();
        let mut with_start: Vec<i32> = state.survivor_starts.keys().copied().collect();
        with_start.sort_unstable();
        assert_eq!(listed, with_start, "survivors and survivor_starts disagree");
    }

    /// T1: a delivered group kill, an escaped job, and an unrelated process
    /// in one turn. The group's members and the unowned process are never
    /// signalled by pid; what the last check still sees is named as running.
    #[tokio::test]
    async fn a_mixed_turn_names_what_the_last_check_saw_and_signals_only_what_it_owns() {
        let (daemon, daemon_sid) = daemon_ids();
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let (g1, g2) = (
            Sleeper::spawn(Some(a), false),
            Sleeper::spawn(Some(a), false),
        );
        let (mut e1, mut e2) = (Sleeper::spawn(None, true), Sleeper::spawn(None, true));
        let mut u = Sleeper::spawn(None, true);
        let (g1p, g2p, e1p, e2p, up) = (g1.pid(), g2.pid(), e1.pid(), e2.pid(), u.pid());
        let reader = scripted_reader(move |call| {
            let mut table = vec![
                sleep_entry(g2p, a, a, daemon_sid, 300),
                sleep_entry(e1p, a, e1p, e1p, 400),
                sleep_entry(up, 1, up, up, 600),
            ];
            // From sweep scan 2 on, A, G1 and E2 are gone.
            if call.is_none_or(|n| n < 2) {
                table.extend([
                    sleep_entry(a, daemon, a, daemon_sid, 100),
                    sleep_entry(g1p, a, a, daemon_sid, 200),
                    sleep_entry(e2p, a, e2p, e2p, 500),
                ]);
            }
            table
        });
        let tracker = sweep_tracker(reader);
        kill_and_sweep(&tracker, a as u32, "test").await;
        let (killed, survivors) = {
            let state = tracker.state();
            assert_paired(&state);
            (pids_of(&state.killed), pids_of(&state.survivors))
        };
        assert!(u.is_running(), "U is not the turn's and is never signalled");
        assert!(!killed.contains(&g1p), "G1 is the group kill's: {killed:?}");
        assert!(survivors.contains(&e1p), "E1 is a survivor: {survivors:?}");
        assert!(!killed.contains(&g2p), "G2 is the group kill's: {killed:?}");
        assert_eq!(sorted(killed), sorted(vec![e1p, e2p]));
        assert_eq!(sorted(survivors), sorted(vec![g2p, e1p]));
        assert_eq!(e1.reap_signal(), Some(libc::SIGKILL));
        assert_eq!(e2.reap_signal(), Some(libc::SIGKILL));
        let (low, high) = (g2p.min(e1p), g2p.max(e1p));
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            killed_note.as_deref(),
            Some(
                format!(
                    "killed 1 process(es) the turn left running: {e2p} (sleep); 2 still running \
                     at the last check: {low} (sleep), {high} (sleep)"
                )
                .as_str()
            )
        );
        assert_eq!(unchecked, None);
    }

    /// T1b: no group signal was delivered (`spare_group` is `None`), and a
    /// same-group process the selector does not own is only observed.
    #[tokio::test]
    async fn an_unowned_group_member_is_listed_but_never_signalled() {
        let (_, daemon_sid) = daemon_ids();
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let mut n = Sleeper::spawn(Some(a), false);
        let np = n.pid();
        let reader = scripted_reader(move |_| vec![sleep_entry(np, 1, a, daemon_sid, 200)]);
        let tracker = sweep_tracker(reader);
        let returned = tracker.sweep(a, None, "test").await;
        assert!(n.is_running(), "N is not owned, so it is never signalled");
        let state = tracker.state();
        assert_paired(&state);
        assert_eq!(pids_of(&state.survivors), vec![np]);
        assert!(state.killed.is_empty(), "{:?}", state.killed);
        assert_eq!(returned, 0);
    }

    /// What a delivered group kill and the tables after it leave behind.
    struct AfterGroupKill {
        a: i32,
        n: i32,
        survivors: Vec<(i32, String)>,
        killed: Vec<(i32, String)>,
        note: Option<String>,
    }

    /// Runs `kill_and_sweep` on a real leader `a` with a real same-group
    /// member `n`; `table(call, a, n)` is what the reader shows (`None` for
    /// the scan before the kill, `Some(k)` for the k-th sweep scan).
    async fn after_a_delivered_group_kill(
        table: impl Fn(Option<usize>, i32, i32) -> Vec<crate::proc_table::ProcEntry>
        + Send
        + Sync
        + 'static,
    ) -> AfterGroupKill {
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let member = Sleeper::spawn(Some(a), false);
        let n = member.pid();
        let tracker = sweep_tracker(scripted_reader(move |call| table(call, a, n)));
        let report = kill_and_sweep(&tracker, a as u32, "test").await;
        assert!(
            matches!(report.group, GroupKill::Killed(_)),
            "{}",
            describe_group_kill(&report.group)
        );
        let (survivors, killed) = {
            let state = tracker.state();
            assert_paired(&state);
            (state.survivors.clone(), state.killed.clone())
        };
        let (note, _) = written_notes(&tracker).await;
        AfterGroupKill {
            a,
            n,
            survivors,
            killed,
            note,
        }
    }

    fn leader_row(a: i32, pgid: i32) -> crate::proc_table::ProcEntry {
        let (daemon, daemon_sid) = daemon_ids();
        sleep_entry(a, daemon, pgid, daemon_sid, 100)
    }

    /// T2(a): the leader stays in the table after the group kill.
    #[tokio::test]
    async fn a_leader_still_in_the_table_after_the_group_kill_is_still_running() {
        let r = after_a_delivered_group_kill(|_, a, _| vec![leader_row(a, a)]).await;
        assert_eq!(pids_of(&r.survivors), vec![r.a], "shape (a)");
        assert!(r.killed.is_empty(), "shape (a): {:?}", r.killed);
        assert_eq!(
            r.note,
            Some(format!(
                "1 still running at the last check: {} (sleep)",
                r.a
            ))
        );
    }

    /// T2(b): the leader stays but outside its group; only its pid matches.
    #[tokio::test]
    async fn a_leader_that_left_its_group_is_still_observed_by_pid() {
        let (daemon, _) = daemon_ids();
        let r = after_a_delivered_group_kill(move |call, a, _| {
            vec![leader_row(a, if call.is_some() { daemon } else { a })]
        })
        .await;
        assert_eq!(pids_of(&r.survivors), vec![r.a], "shape (b)");
        assert!(r.killed.is_empty(), "shape (b): {:?}", r.killed);
        assert_eq!(
            r.note,
            Some(format!(
                "1 still running at the last check: {} (sleep)",
                r.a
            ))
        );
    }

    /// T2(c): only an unowned member of the group stays.
    #[tokio::test]
    async fn an_unowned_group_member_still_in_the_table_is_still_running() {
        let (_, daemon_sid) = daemon_ids();
        let r = after_a_delivered_group_kill(move |call, a, n| {
            let member = sleep_entry(n, 1, a, daemon_sid, 200);
            match call {
                None => vec![leader_row(a, a), member],
                Some(_) => vec![member],
            }
        })
        .await;
        assert_eq!(pids_of(&r.survivors), vec![r.n], "shape (c)");
        assert!(r.killed.is_empty(), "shape (c): {:?}", r.killed);
        assert_eq!(
            r.note,
            Some(format!(
                "1 still running at the last check: {} (sleep)",
                r.n
            ))
        );
    }

    /// T2(d): the leader leaves at the second sweep scan.
    #[tokio::test]
    async fn a_leader_that_leaves_by_the_second_check_is_not_listed() {
        let r = after_a_delivered_group_kill(|call, a, _| match call {
            None | Some(1) => vec![leader_row(a, a)],
            Some(_) => Vec::new(),
        })
        .await;
        assert!(r.survivors.is_empty(), "shape (d): {:?}", r.survivors);
        assert!(r.killed.is_empty(), "shape (d): {:?}", r.killed);
        assert_eq!(r.note, None, "shape (d)");
    }

    /// T2(e): only a zombie leader row remains.
    #[tokio::test]
    async fn a_zombie_leader_row_is_not_listed() {
        let r = after_a_delivered_group_kill(|call, a, _| {
            let mut row = leader_row(a, a);
            row.zombie = call.is_some();
            vec![row]
        })
        .await;
        assert!(r.survivors.is_empty(), "shape (e): {:?}", r.survivors);
        assert_eq!(r.note, None, "shape (e)");
    }

    // --- what the logs say ------------------------------------------------------

    use crate::test_support::LogBuf;

    /// Captures this thread's log, DEBUG and above, through the shared
    /// helper whose `KEEPER` stops another thread reaching a log line first
    /// from hiding it (`#[tokio::test]` runs on this thread, so parallel
    /// tests cannot see each other's lines).
    fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
        let buf = LogBuf::default();
        let guard = buf.capture_at(tracing::Level::DEBUG);
        (buf, guard)
    }

    const NOT_VERIFIED: &str = "the turn's cleanup is not verified: a process was still running \
         at the last check, or the process table could not be read";

    /// Writes the notes of a tracker holding these findings, with the log
    /// captured; returns the log.
    async fn log_of_writing_notes(
        survivors: &[(i32, &str)],
        failures: &[&str],
        group_errors: &[&str],
        killed: &[(i32, &str)],
    ) -> String {
        let tracker = sweep_tracker(scripted_reader(|_| Vec::new()));
        {
            let mut state = tracker.state();
            for (pid, comm) in survivors {
                state.add_survivor(*pid, comm.to_string(), 10);
            }
            state.scan_failures = failures.iter().map(|s| s.to_string()).collect();
            state.group_kill_errors = group_errors.iter().map(|s| s.to_string()).collect();
            state.killed = killed.iter().map(|(p, c)| (*p, c.to_string())).collect();
        }
        let (log, _guard) = capture_logs();
        written_notes(&tracker).await;
        log.text()
    }

    /// The verification warning fires for survivors and for failed scans, with
    /// their counts, and for nothing else.
    #[tokio::test]
    async fn the_not_verified_warning_follows_survivors_and_failed_scans() {
        let log = log_of_writing_notes(&[(5, "a"), (6, "b")], &[], &[], &[]).await;
        assert!(log.contains(NOT_VERIFIED), "survivors only: {log}");
        assert!(log.contains("still_running=2"), "survivors only: {log}");
        assert!(log.contains("failed_scans=0"), "survivors only: {log}");
        assert!(log.contains("session_id="), "{log}");

        let log = log_of_writing_notes(&[], &["site: boom"], &[], &[]).await;
        assert!(log.contains(NOT_VERIFIED), "failed scan only: {log}");
        assert!(log.contains("still_running=0"), "failed scan only: {log}");
        assert!(log.contains("failed_scans=1"), "failed scan only: {log}");

        let log = log_of_writing_notes(&[(5, "a")], &["s: x", "s: y"], &["s: g"], &[]).await;
        assert!(log.contains("still_running=1"), "both: {log}");
        assert!(log.contains("failed_scans=2"), "both: {log}");

        // Neither a delivered kill, nor a group error alone, is a warning.
        let log = log_of_writing_notes(&[], &[], &["cancel: EPERM"], &[(7, "c")]).await;
        assert!(!log.contains(NOT_VERIFIED), "no findings to doubt: {log}");
        let log = log_of_writing_notes(&[], &[], &[], &[]).await;
        assert!(!log.contains(NOT_VERIFIED), "nothing at all: {log}");
    }

    /// At the last round the warning counts the deduplicated union of what is
    /// owned and what is alive in the group, once each.
    #[tokio::test]
    async fn the_last_round_warning_counts_each_live_process_once() {
        let (a, n, x) = (NO_AGENT, NO_AGENT + 1, NO_AGENT + 2);
        let (_, daemon_sid) = daemon_ids();
        // Leader, an unowned member, and a marker-owned member: three
        // processes; the owned one is in both the owned list and the group.
        let reader = scripted_reader(move |_| {
            vec![
                leader_row(a, a),
                sleep_entry(n, 1, a, daemon_sid, 200),
                marked(sleep_entry(x, 1, a, daemon_sid, 300)),
            ]
        });
        let tracker = sweep_tracker(reader);
        let (log, _guard) = capture_logs();
        let returned = tracker.sweep(a, Some(a), "grace site").await;
        let log = log.text();
        assert_eq!(returned, 0);
        assert!(
            log.contains("still running at the sweep's last check"),
            "{log}"
        );
        assert!(log.contains("count=3"), "{log}");
        assert!(log.contains("site=\"grace site\""), "{log}");
        assert!(
            !log.contains("sent SIGKILL to processes left behind"),
            "nothing was signalled by pid: {log}"
        );
    }

    /// An ordinary empty finish is silent; a delivered kill is reported with
    /// its count and is not a last-round warning.
    #[tokio::test]
    async fn the_sweep_warns_only_about_what_it_saw_or_signalled() {
        let tracker = sweep_tracker(scripted_reader(|_| Vec::new()));
        let (log, guard) = capture_logs();
        tracker.sweep(NO_AGENT, None, "quiet").await;
        let quiet = log.text();
        drop(guard);
        assert!(!quiet.contains("last check"), "{quiet}");
        assert!(!quiet.contains("left behind"), "{quiet}");

        let mut e = Sleeper::spawn(None, true);
        let ep = e.pid();
        let reader = scripted_reader(move |call| match call {
            None | Some(1) => vec![marked(sleep_entry(ep, 1, ep, ep, 400))],
            Some(_) => Vec::new(),
        });
        let tracker = sweep_tracker(reader);
        let (log, _guard) = capture_logs();
        let returned = tracker.sweep(NO_AGENT, None, "end of turn").await;
        let loud = log.text();
        assert_eq!(returned, 1);
        assert_eq!(e.reap_signal(), Some(libc::SIGKILL));
        assert!(
            loud.contains("sent SIGKILL to processes left behind by the turn"),
            "{loud}"
        );
        assert!(loud.contains("signalled=1"), "{loud}");
        assert!(loud.contains("site=\"end of turn\""), "{loud}");
        assert!(!loud.contains("last check"), "it was gone by then: {loud}");
    }

    /// The group-kill description and the shared debug line say a signal was
    /// delivered or tried, never that the group is dead.
    #[tokio::test]
    async fn the_group_kill_log_claims_a_signal_not_a_death() {
        assert_eq!(
            describe_group_kill(&GroupKill::Killed(3)),
            "signalled 3 live member(s)"
        );
        let leader = Sleeper::spawn(None, false);
        let a = leader.pid();
        let reader = scripted_reader(move |call| match call {
            None => vec![leader_row(a, a)],
            Some(_) => Vec::new(),
        });
        let tracker = sweep_tracker(reader);
        let (log, _guard) = capture_logs();
        kill_and_sweep(&tracker, a as u32, "end of turn").await;
        let log = log.text();
        assert!(
            log.contains("tried to signal the agent's group and swept"),
            "{log}"
        );
        assert!(log.contains("signalled 1 live member(s)"), "{log}");
        assert!(log.contains("site=\"end of turn\""), "{log}");
    }

    // --- one tracker, several sites ------------------------------------------

    /// An agent pid no table lists, so only `spare_group` and ownership matter.
    const NO_AGENT: i32 = 99_999_990;
    const X_START: u64 = 777;

    /// X is a real process the first site lists as a survivor: marker-only
    /// ownership (ppid 1, not a descendant), watched in its own spared
    /// group. `later(x, scan)` is what the reader shows once a test calls
    /// `Phase::next`.
    async fn x_listed_at_a_first_site(
        later: impl Fn(i32, usize) -> ScanResult + Send + Sync + 'static,
    ) -> (Sleeper, LeftoverTracker, Arc<Phase>) {
        let (_, daemon_sid) = daemon_ids();
        let x = Sleeper::spawn(None, false);
        let xp = x.pid();
        let (reader, phase) = phased_reader(move |phase, scan| match phase {
            0 => Ok(vec![marked(sleep_entry(xp, 1, xp, daemon_sid, X_START))]),
            _ => later(xp, scan),
        });
        let tracker = sweep_tracker(reader);
        tracker.sweep(NO_AGENT, Some(xp), "site 1").await;
        {
            let state = tracker.state();
            assert_eq!(state.survivors, vec![(xp, "sleep".to_string())]);
            assert_eq!(state.survivor_starts, HashMap::from([(xp, X_START)]));
            assert!(state.killed.is_empty());
        }
        phase.next();
        (x, tracker, phase)
    }

    /// T3(a): X is absent from the second site's table.
    #[tokio::test]
    async fn a_survivor_gone_by_the_next_site_is_cleared() {
        let (mut x, tracker, _) = x_listed_at_a_first_site(|_, _| Ok(Vec::new())).await;
        tracker.sweep(NO_AGENT, None, "site 2").await;
        let state = tracker.state();
        assert!(state.survivors.is_empty(), "{:?}", state.survivors);
        assert!(
            state.survivor_starts.is_empty(),
            "{:?}",
            state.survivor_starts
        );
        assert!(x.is_running());
    }

    /// T3(b): X is still there with the same start, but nothing owns it now:
    /// it stays listed, with its identity, and is not signalled.
    #[tokio::test]
    async fn a_survivor_that_lost_its_ownership_evidence_stays_listed() {
        let (_, daemon_sid) = daemon_ids();
        let (mut x, tracker, _) = x_listed_at_a_first_site(move |xp, _| {
            Ok(vec![sleep_entry(xp, 1, xp, daemon_sid, X_START)])
        })
        .await;
        let returned = tracker.sweep(NO_AGENT, None, "site 2").await;
        assert!(x.is_running(), "an unowned process is never signalled");
        let state = tracker.state();
        let xp = x.pid();
        assert_eq!(state.survivors, vec![(xp, "sleep".to_string())]);
        assert_eq!(state.survivor_starts, HashMap::from([(xp, X_START)]));
        assert_eq!(returned, 0);
    }

    /// T3(c): the pid is back with another start time and no ownership: a
    /// different process, so the old survivor is cleared and nothing is sent.
    #[tokio::test]
    async fn a_reused_pid_does_not_keep_the_old_survivor() {
        let (_, daemon_sid) = daemon_ids();
        let (mut x, tracker, _) = x_listed_at_a_first_site(move |xp, _| {
            Ok(vec![sleep_entry(xp, 1, xp, daemon_sid, X_START + 111)])
        })
        .await;
        tracker.sweep(NO_AGENT, None, "site 2").await;
        assert!(x.is_running(), "an unowned process is never signalled");
        let state = tracker.state();
        assert!(state.survivors.is_empty(), "{:?}", state.survivors);
        assert!(
            state.survivor_starts.is_empty(),
            "{:?}",
            state.survivor_starts
        );
        assert!(state.killed.is_empty());
    }

    /// A zombie row is not alive: it clears a survivor of an earlier site.
    #[tokio::test]
    async fn a_later_zombie_clears_a_survivor() {
        let (_, daemon_sid) = daemon_ids();
        let (_x, tracker, _) = x_listed_at_a_first_site(move |xp, _| {
            let mut row = marked(sleep_entry(xp, 1, xp, daemon_sid, X_START));
            row.zombie = true;
            Ok(vec![row])
        })
        .await;
        tracker.sweep(NO_AGENT, None, "site 2").await;
        let state = tracker.state();
        assert!(state.survivors.is_empty(), "{:?}", state.survivors);
        assert!(state.survivor_starts.is_empty());
    }

    /// A different process at the same pid that the selector does own is
    /// listed as a new survivor, with its own start.
    #[tokio::test]
    async fn a_new_identity_at_a_reused_pid_replaces_the_old_one() {
        let (_, daemon_sid) = daemon_ids();
        let (x, tracker, _) = x_listed_at_a_first_site(move |xp, _| {
            let mut row = marked(sleep_entry(xp, 1, xp, daemon_sid, X_START + 111));
            row.comm = "reused".into();
            Ok(vec![row])
        })
        .await;
        tracker.sweep(NO_AGENT, Some(x.pid()), "site 2").await;
        let state = tracker.state();
        assert_eq!(state.survivors, vec![(x.pid(), "reused".to_string())]);
        assert_eq!(
            state.survivor_starts,
            HashMap::from([(x.pid(), X_START + 111)])
        );
    }

    /// T3(d): the signal is delivered, and the next scan fails. A signal is
    /// not a death and a failed read is not an observation: X stays listed.
    #[tokio::test]
    async fn a_delivered_signal_followed_by_a_failed_read_leaves_the_survivor_listed() {
        let (_, daemon_sid) = daemon_ids();
        let (mut x, tracker, _) = x_listed_at_a_first_site(move |xp, scan| match scan {
            0 => Ok(vec![marked(sleep_entry(xp, 1, xp, daemon_sid, X_START))]),
            _ => Err(std::io::Error::other("second read failed")),
        })
        .await;
        let xp = x.pid();
        let returned = tracker.sweep(NO_AGENT, None, "site 2").await;
        {
            let state = tracker.state();
            assert_eq!(state.survivors, vec![(xp, "sleep".to_string())]);
            assert_paired(&state);
            assert_eq!(state.killed, vec![(xp, "sleep".to_string())]);
            assert_eq!(state.scan_failures, vec!["site 2: second read failed"]);
        }
        assert_eq!(returned, 1);
        assert_eq!(x.reap_signal(), Some(libc::SIGKILL));
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!("1 still running at the last check: {xp} (sleep)"))
        );
        assert!(unchecked.is_some());
    }

    /// Survivors and their identities move together.
    #[test]
    fn survivor_identities_stay_paired_with_survivors() {
        let live = |pid, start, comm: &str| {
            let mut entry = sleep_entry(pid, 1, pid, pid, start);
            entry.comm = comm.into();
            entry
        };
        let mut state = TrackerState::default();
        state.add_survivor(10, "a".into(), 1000);
        state.add_survivor(20, "b".into(), 2000);
        state.add_survivor(10, "dup".into(), 9999);
        assert_eq!(
            state.survivors,
            vec![(10, "a".to_string()), (20, "b".to_string())]
        );
        assert_eq!(
            state.survivor_starts,
            HashMap::from([(10, 1000), (20, 2000)]),
            "a missing start"
        );
        assert_paired(&state);

        // Pid 10 left the table: cleared from both.
        state.settle_survivors(&[live(20, 2000, "b")], &[]);
        assert_eq!(state.survivors, vec![(20, "b".to_string())]);
        assert_eq!(
            state.survivor_starts,
            HashMap::from([(20, 2000)]),
            "a stale start"
        );

        // Pid 20 came back as another process the check calls alive.
        state.settle_survivors(&[live(20, 2500, "new")], &[20]);
        assert_eq!(state.survivors, vec![(20, "new".to_string())]);
        assert_eq!(state.survivor_starts, HashMap::from([(20, 2500)]));

        // A survivor with no recorded identity cannot be matched to anything.
        state.survivors.push((30, "c".into()));
        state.settle_survivors(&[live(30, 0, "c"), live(20, 2500, "new")], &[]);
        assert_eq!(state.survivors, vec![(20, "new".to_string())]);
        assert_paired(&state);

        // Start time 0 is a value, not a wildcard.
        state.add_survivor(40, "z".into(), 0);
        state.settle_survivors(&[live(40, 5, "z")], &[]);
        assert!(!pids_of(&state.survivors).contains(&40));
        assert_paired(&state);
    }

    /// Writing the notes consumes survivors and their identities together
    /// and leaves the recorded ownership alone.
    #[tokio::test]
    async fn writing_the_notes_consumes_survivors_with_their_identities() {
        let tracker = sweep_tracker(Arc::new(|_| Ok(Vec::new())));
        {
            let mut state = tracker.state();
            state.recorded.insert((77, 7));
            state.add_survivor(10, "a".into(), 1000);
        }
        let (killed_note, _) = written_notes(&tracker).await;
        assert_eq!(
            killed_note.as_deref(),
            Some("1 still running at the last check: 10 (a)")
        );
        let state = tracker.state();
        assert!(state.survivors.is_empty());
        assert!(
            state.survivor_starts.is_empty(),
            "{:?}",
            state.survivor_starts
        );
        assert!(state.recorded.contains(&(77, 7)));
    }

    /// The owner's round, item 2: SIGKILL was delivered by pid to X while the
    /// selector owned it (marker present on sweep 1); on sweep 2 X has lost
    /// its marker, so nothing owns it, but it is still in the table with the
    /// same start. The signal is not a death: X is a survivor and not in the
    /// killed clause.
    #[tokio::test]
    async fn a_signalled_process_the_selector_no_longer_owns_is_a_survivor_while_live() {
        const X_START: u64 = 400;
        let mut x = Sleeper::spawn(None, true);
        let xp = x.pid();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![marked(sleep_entry(xp, 1, xp, xp, X_START))],
            _ => vec![sleep_entry(xp, 1, xp, xp, X_START)],
        });
        let tracker = sweep_tracker(reader);
        let returned = tracker.sweep(NO_AGENT, None, "end of turn").await;
        assert_eq!(returned, 1);
        assert_eq!(x.reap_signal(), Some(libc::SIGKILL));
        {
            let state = tracker.state();
            assert_eq!(
                pids_of(&state.survivors),
                vec![xp],
                "a signalled pid still live by pid and start time is a survivor"
            );
            assert_eq!(state.survivor_starts, HashMap::from([(xp, X_START)]));
            assert_eq!(
                pids_of(&state.killed),
                vec![xp],
                "the signal is still recorded"
            );
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!("1 still running at the last check: {xp} (sleep)"))
        );
        assert_eq!(unchecked, None);
    }

    /// The same sequence, but the later scan shows a different start at that
    /// pid (the number was reused): the earlier process is gone, so the
    /// signalled pid is not a survivor.
    #[tokio::test]
    async fn a_signalled_pid_reused_by_another_process_is_not_a_survivor() {
        let mut x = Sleeper::spawn(None, true);
        let xp = x.pid();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![marked(sleep_entry(xp, 1, xp, xp, 400))],
            _ => vec![sleep_entry(xp, 1, xp, xp, 999)],
        });
        let tracker = sweep_tracker(reader);
        tracker.sweep(NO_AGENT, None, "end of turn").await;
        assert_eq!(x.reap_signal(), Some(libc::SIGKILL));
        {
            let state = tracker.state();
            assert!(state.survivors.is_empty(), "{:?}", state.survivors);
        }
        let (killed_note, _) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!(
                "killed 1 process(es) the turn left running: {xp} (sleep)"
            ))
        );
    }

    /// Internal review, finding 1: one tracker, two sites. Site 1 signals X
    /// (start 400) and its final scan is empty. At site 2 the pid is another
    /// process (start 999), the selector owns it on the first scan and signals
    /// it, then it loses its marker. The final scan still shows that second
    /// identity, so it is a survivor: the map must hold the latest signalled
    /// start, not the first.
    #[tokio::test]
    async fn a_reused_pid_signalled_again_is_matched_by_its_new_start() {
        let mut x = Sleeper::spawn(None, true);
        let xp = x.pid();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![marked(sleep_entry(xp, 1, xp, xp, 400))],
            Some(2) => Vec::new(),
            Some(3) => vec![marked(sleep_entry(xp, 1, xp, xp, 999))],
            _ => vec![sleep_entry(xp, 1, xp, xp, 999)],
        });
        let tracker = sweep_tracker(reader);
        assert_eq!(tracker.sweep(NO_AGENT, None, "site one").await, 1);
        assert!(tracker.state().survivors.is_empty());
        // Site 2: signalling a pid the turn already counted for the grace
        // decision (its policy-counted history, kept by pid) adds nothing to
        // the count the grace decision reads, though `killed_starts` still
        // takes the new identity.
        assert_eq!(tracker.sweep(NO_AGENT, None, "site two").await, 0);
        {
            let state = tracker.state();
            assert_eq!(
                pids_of(&state.survivors),
                vec![xp],
                "the second identity at this pid is still live"
            );
            assert_eq!(state.survivor_starts, HashMap::from([(xp, 999)]));
            assert_eq!(state.killed_starts, HashMap::from([(xp, 999)]));
        }
        assert_eq!(x.reap_signal(), Some(libc::SIGKILL));
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!("1 still running at the last check: {xp} (sleep)"))
        );
        assert_eq!(unchecked, None);
    }

    /// Internal review, finding 3: a signalled pid whose last row is a zombie
    /// has exited, so it is killed, not a survivor. Only the signalled
    /// identity's own matcher can say this: nothing was listed before.
    #[tokio::test]
    async fn a_signalled_process_that_is_a_zombie_at_the_last_check_is_gone() {
        let mut x = Sleeper::spawn(None, true);
        let xp = x.pid();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![marked(sleep_entry(xp, 1, xp, xp, 400))],
            _ => {
                let mut entry = sleep_entry(xp, 1, xp, xp, 400);
                entry.zombie = true;
                vec![entry]
            }
        });
        let tracker = sweep_tracker(reader);
        tracker.sweep(NO_AGENT, None, "end of turn").await;
        assert_eq!(x.reap_signal(), Some(libc::SIGKILL));
        {
            let state = tracker.state();
            assert!(
                state.survivors.is_empty(),
                "a signalled zombie is gone, not a survivor: {:?}",
                state.survivors
            );
        }
        let (killed_note, _) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!(
                "killed 1 process(es) the turn left running: {xp} (sleep)"
            ))
        );
    }

    /// Internal review, finding 2: the warning counts every process a sweep
    /// signalled, including those found by the rounds that only wait for the
    /// leader, while the returned total (read by the grace decision) does not.
    #[tokio::test]
    async fn the_signal_warning_counts_late_deliveries_the_total_does_not() {
        let leader = Sleeper::spawn(None, false);
        let lp = leader.pid();
        let leader_entry = move || sleep_entry(lp, 1, lp, lp, 10);
        let mut late = Sleeper::spawn(None, true);
        let late_pid = late.pid();
        let mut early = Sleeper::spawn(None, true);
        let early_pid = early.pid();
        let mut later = Sleeper::spawn(None, true);
        let later_pid = later.pid();

        // Only a late delivery: the first table has just the leader.
        let (log, _guard) = capture_logs();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![leader_entry()],
            Some(2) => vec![
                leader_entry(),
                marked(sleep_entry(late_pid, 1, late_pid, late_pid, 400)),
            ],
            _ => Vec::new(),
        });
        let total = sweep_tracker(reader).sweep(lp, None, "late only").await;
        assert_eq!(total, 0, "the policy total ignores the late delivery");
        assert_eq!(late.reap_signal(), Some(libc::SIGKILL));
        let text = log.text();
        let line = text
            .lines()
            .find(|l| l.contains("sent SIGKILL to processes left behind"))
            .unwrap_or_else(|| panic!("no signal warning: {text}"));
        assert!(line.contains("signalled=1"), "{line}");

        // One early and one late delivery.
        let (log, _guard) = capture_logs();
        let reader = scripted_reader(move |call| match call {
            Some(1) => vec![
                leader_entry(),
                marked(sleep_entry(early_pid, 1, early_pid, early_pid, 500)),
            ],
            Some(2) => vec![leader_entry()],
            Some(3) => vec![
                leader_entry(),
                marked(sleep_entry(later_pid, 1, later_pid, later_pid, 600)),
            ],
            _ => Vec::new(),
        });
        let total = sweep_tracker(reader)
            .sweep(lp, None, "early and late")
            .await;
        assert_eq!(total, 1, "only the delivery before the old stop counts");
        assert_eq!(early.reap_signal(), Some(libc::SIGKILL));
        assert_eq!(later.reap_signal(), Some(libc::SIGKILL));
        let text = log.text();
        let line = text
            .lines()
            .find(|l| l.contains("sent SIGKILL to processes left behind"))
            .unwrap_or_else(|| panic!("no signal warning: {text}"));
        assert!(line.contains("signalled=2"), "{line}");
    }

    /// The guard that keeps the late-discovery tests from leaking their job
    /// kills the process named in its pid file, and does nothing for a file
    /// that was never written.
    #[test]
    fn the_pid_file_guard_kills_the_process_it_names() {
        let dir = TempDir::new();
        let file = dir.0.join("job.pid");
        let mut sleeper = Sleeper::spawn(None, true);
        std::fs::write(&file, sleeper.pid().to_string()).unwrap();
        drop(crate::test_support::KillPidFileOnDrop(file));
        assert_eq!(sleeper.reap_signal(), Some(libc::SIGKILL));
        drop(crate::test_support::KillPidFileOnDrop(
            dir.0.join("none.pid"),
        ));
    }

    /// The owner's round, item 4: a contender that looks at the tracker at
    /// the start of every lock acquisition, where another writer could run,
    /// never sees a partly consumed state. Every finding (killed, survivors
    /// and both identity maps, failed scans, group errors) leaves together,
    /// with the policy-counted pids and the pending late kills. Recorded
    /// ownership is not consumed.
    #[tokio::test]
    async fn the_notes_consume_every_finding_under_one_lock() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let tracker = sweep_tracker(Arc::new(|_| Ok(Vec::new())));
        {
            let mut state = tracker.state();
            state.killed.push((10, "k".into()));
            state.killed_starts.insert(10, 100);
            state.policy_counted_pids.insert(10);
            state.late_killed_pids.push(40);
            state.recorded.insert((30, 300));
            state.add_survivor(20, "s".into(), 200);
            state.scan_failures.push("end of turn: read failed".into());
            state
                .group_kill_errors
                .push("end of turn: signal failed".into());
        }
        let looks = Arc::new(AtomicUsize::new(0));
        let torn = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (seen_looks, seen_torn) = (Arc::clone(&looks), Arc::clone(&torn));
        let hook: LockHook = Box::new(move |state| {
            seen_looks.fetch_add(1, SeqCst);
            let state = state.lock().unwrap();
            let present = [
                !state.killed.is_empty(),
                !state.killed_starts.is_empty(),
                !state.survivors.is_empty(),
                !state.survivor_starts.is_empty(),
                !state.scan_failures.is_empty(),
                !state.group_kill_errors.is_empty(),
                !state.policy_counted_pids.is_empty(),
                !state.late_killed_pids.is_empty(),
            ];
            if present.iter().any(|p| *p) && !present.iter().all(|p| *p) {
                seen_torn.lock().unwrap().push(format!("{present:?}"));
            }
        });
        assert!(tracker.before_lock.set(hook).is_ok());
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert!(looks.load(SeqCst) >= 1, "the contender never looked");
        assert_eq!(
            *torn.lock().unwrap(),
            Vec::<String>::new(),
            "a contender saw some findings consumed and others not"
        );
        assert!(killed_note.is_some() && unchecked.is_some());
        let state = tracker.state.lock().unwrap();
        assert!(
            state.killed.is_empty()
                && state.killed_starts.is_empty()
                && state.survivors.is_empty()
                && state.survivor_starts.is_empty()
                && state.scan_failures.is_empty()
                && state.group_kill_errors.is_empty()
                && state.policy_counted_pids.is_empty()
                && state.late_killed_pids.is_empty()
        );
        assert_eq!(
            state.recorded,
            std::collections::HashSet::from([(30, 300)]),
            "recorded ownership is not a consumed finding"
        );
    }

    /// The owner's round, item 1, at session level: the first sweep table
    /// holds only the leader, the next reveals an owned escaped job. The extra
    /// round that waits for the leader still kills the job, but the grace
    /// result is what it was when the loop stopped at the first table: nothing
    /// lingered, and the turn ends as it did.
    #[tokio::test]
    async fn a_job_found_by_the_rounds_that_wait_for_the_leader_does_not_make_the_turn_linger() {
        let dir = TempDir::new();
        let leader_file = dir.0.join("leader.pid");
        let job_file = dir.0.join("job.pid");
        let _job_guard = crate::test_support::KillPidFileOnDrop(job_file.clone());
        let binary = script_binary(
            &dir.0,
            crate::test_support::escaped_job_turn_steps(&leader_file, &job_file),
        );
        let reader = crate::test_support::late_discovery_reader(leader_file);
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, None);
        assert_eq!(run.status, SessionStatus::Idle);
        let notes = session_notes(&pool, &session_id).await;
        assert!(!notes.contains(&"lingered".to_string()), "{notes:?}");
        let job = read_pid_when_written(&job_file).await;
        wait_until_gone(job).await;
    }

    /// The owner's round, item 3: the real grace caller, with a leader that
    /// has already exited (an unreaped zombie), a pre-kill read that fails and
    /// an empty final check. Nothing was seen running, so the warning claims
    /// an attempt and an unavailable check, never a running process. The turn
    /// still ends Lingered, as it always did.
    #[tokio::test]
    async fn the_grace_warning_does_not_claim_a_process_was_seen_running() {
        struct KillOnDrop(PathBuf);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if let Some(pid) = std::fs::read_to_string(&self.0)
                    .ok()
                    .and_then(|t| t.trim().parse::<i32>().ok())
                {
                    // SAFETY: only signals the test's own stray sleep.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let (log, _guard) = capture_logs();
        let dir = TempDir::new();
        let stray_file = dir.0.join("stray.pid");
        let _stray = KillOnDrop(stray_file.clone());
        // A job in its own session that holds the output pipe open, so the
        // stream does not end when the agent exits and the grace timer fires.
        let spawn = format!(
            "python3 -c 'import subprocess; p = subprocess.Popen([\"sleep\", \"30\"], \
             start_new_session=True); open(\"{}\", \"w\").write(str(p.pid))'",
            stray_file.display()
        );
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "run", "command": spawn},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        );
        let reader: crate::proc_table::ProcReader = Arc::new(|marker| match marker {
            None => Err(std::io::Error::other("injected pre-kill failure")),
            Some(_) => Ok(Vec::new()),
        });
        let (pool, session_id, _manager) = start_leftovers(
            binary,
            fast_timers(3),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(
            (run.status, run.end_reason),
            (SessionStatus::Exited, Some(SessionEndReason::Lingered))
        );
        let text = log.text();
        let line = text
            .lines()
            .find(|l| l.contains("grace period over"))
            .unwrap_or_else(|| panic!("no grace warning in: {text}"));
        assert!(line.contains("WARN"), "{line}");
        assert!(
            line.contains(
                "tried to signal the agent's process group and sweep what the turn started; \
                 this does not say a process was seen running"
            ),
            "{line}"
        );
        assert!(
            !text.contains("process still running after stdin was closed"),
            "{text}"
        );
    }

    // --- a table that cannot be read ----------------------------------------

    /// T4(a): the first sweep scan shows an owned E, the second fails.
    #[tokio::test]
    async fn a_failed_scan_after_a_kill_says_the_kill_is_unconfirmed() {
        let mut e = Sleeper::spawn(None, true);
        let ep = e.pid();
        let reader = fallible_reader(move |call| match call {
            Some(1) => Ok(vec![marked(sleep_entry(ep, 1, ep, ep, 400))]),
            _ => Err(std::io::Error::other("table read failed")),
        });
        let tracker = sweep_tracker(reader);
        let returned = tracker.sweep(NO_AGENT, None, "end of turn").await;
        assert_eq!(returned, 1);
        assert_eq!(e.reap_signal(), Some(libc::SIGKILL));
        {
            let state = tracker.state();
            assert_eq!(state.killed, vec![(ep, "sleep".to_string())]);
            assert!(state.survivors.is_empty(), "{:?}", state.survivors);
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            unchecked.as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: end of turn: table read failed"
            ),
            "the unchecked note"
        );
        assert_eq!(
            killed_note,
            Some(format!(
                "sent SIGKILL to 1 process(es) the turn left running, but could not confirm \
                 they are gone: {ep} (sleep)"
            )),
            "the downgraded wording"
        );
    }

    /// T4(b): the pre-kill scan fails and every later scan works, at two
    /// sites of one tracker. The failure stays.
    #[tokio::test]
    async fn an_early_failed_read_is_not_recovered_by_later_good_ones() {
        let (first, second) = (Sleeper::spawn(None, false), Sleeper::spawn(None, false));
        let reader = counting_reader(|scan| match scan {
            0 => Err(std::io::Error::other("pre-kill read failed")),
            _ => Ok(Vec::new()),
        });
        let tracker = sweep_tracker(reader);
        kill_and_sweep(&tracker, first.pid() as u32, "cancel").await;
        kill_and_sweep(&tracker, second.pid() as u32, "end of turn").await;
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(killed_note, None);
        assert_eq!(
            unchecked.as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: cancel: pre-kill read failed"
            )
        );
    }

    /// T4(c): every scan fails.
    #[tokio::test]
    async fn when_every_read_fails_only_the_unchecked_note_is_written() {
        let leader = Sleeper::spawn(None, false);
        let reader =
            counting_reader(|scan| Err(std::io::Error::other(format!("read {scan} failed"))));
        let tracker = sweep_tracker(reader);
        kill_and_sweep(&tracker, leader.pid() as u32, "end of turn").await;
        {
            let state = tracker.state();
            assert!(state.killed.is_empty() && state.survivors.is_empty());
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(killed_note, None);
        assert_eq!(
            unchecked.as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: end of turn: read 0 failed; end of turn: read 1 failed"
            )
        );
    }

    /// A turn-event read that fails is as sticky as one inside a kill site,
    /// and a later good site does not undo it.
    #[tokio::test]
    async fn a_failed_turn_event_read_downgrades_the_later_kill_wording() {
        let leader = Sleeper::spawn(None, false);
        let mut e = Sleeper::spawn(None, true);
        let ep = e.pid();
        let reader = fallible_reader(move |call| match call {
            None => Ok(Vec::new()),
            Some(1) => Ok(vec![marked(sleep_entry(ep, 1, ep, ep, 400))]),
            Some(_) => Ok(Vec::new()),
        });
        // Call 0 is the turn-event scan, the only one that fails.
        let failing_first = {
            let first = std::sync::atomic::AtomicBool::new(true);
            Arc::new(move |marker: Option<&str>| {
                if marker.is_none() && first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    Err(std::io::Error::other("turn event read failed"))
                } else {
                    reader(marker)
                }
            }) as crate::proc_table::ProcReader
        };
        let tracker = sweep_tracker(failing_first);
        tracker
            .record_descendants(leader.pid() as u32, "turn event")
            .await;
        kill_and_sweep(&tracker, leader.pid() as u32, "end of turn").await;
        assert_eq!(e.reap_signal(), Some(libc::SIGKILL));
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            unchecked.as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: turn event: turn event read failed"
            )
        );
        assert_eq!(
            killed_note,
            Some(format!(
                "sent SIGKILL to 1 process(es) the turn left running, but could not confirm \
                 they are gone: {ep} (sleep)"
            ))
        );
    }

    // --- the note writer and the group-kill errors ----------------------------

    fn named(items: &[(i32, &str)]) -> Vec<(i32, String)> {
        items.iter().map(|(p, c)| (*p, c.to_string())).collect()
    }

    fn texts(
        killed: &[(i32, &str)],
        survivors: &[(i32, &str)],
        failures: &[&str],
        group_errors: &[&str],
    ) -> (Option<String>, Option<String>) {
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        leftover_note_texts(
            &named(killed),
            &named(survivors),
            &strings(failures),
            &strings(group_errors),
        )
    }

    /// T5: a pid in both lists is named once, as still running.
    #[test]
    fn a_pid_in_both_lists_is_named_once_as_still_running() {
        let (killed, unchecked) = texts(&[(5, "a")], &[(5, "a")], &[], &[]);
        assert_eq!(
            killed.as_deref(),
            Some("1 still running at the last check: 5 (a)")
        );
        assert_eq!(unchecked, None);
        let (killed, _) = texts(&[(5, "a"), (6, "b")], &[(5, "a")], &[], &[]);
        assert_eq!(
            killed.as_deref(),
            Some(
                "killed 1 process(es) the turn left running: 6 (b); 1 still running at the last check: 5 (a)"
            )
        );
    }

    /// T5: each clause is sorted by number, not by text, and the overlap is
    /// filtered before the killed count.
    #[test]
    fn each_clause_is_sorted_by_pid() {
        let (killed, _) = texts(
            &[(100, "d"), (9, "a"), (50, "x"), (20, "b")],
            &[(300, "f"), (50, "x"), (45, "e"), (1000, "g")],
            &[],
            &[],
        );
        assert_eq!(
            killed.as_deref(),
            Some(
                "killed 3 process(es) the turn left running: 9 (a), 20 (b), 100 (d); \
                 4 still running at the last check: 45 (e), 50 (x), 300 (f), 1000 (g)"
            )
        );
    }

    /// T5: group errors beside a survivor, beside a failed scan, and alone.
    #[test]
    fn group_errors_appear_only_beside_a_survivor_or_a_failed_scan() {
        let (killed, unchecked) = texts(
            &[],
            &[(5, "a")],
            &[],
            &["cancel: eperm", "end of turn: eperm"],
        );
        assert_eq!(
            killed.as_deref(),
            Some(
                "1 still running at the last check: 5 (a); the kill of the agent's process \
                 group failed: cancel: eperm; end of turn: eperm"
            )
        );
        assert_eq!(unchecked, None);

        let (killed, unchecked) = texts(&[], &[], &["grace: read failed"], &["grace: eperm"]);
        assert_eq!(killed, None);
        let unchecked = unchecked.unwrap();
        assert_eq!(
            unchecked,
            "the process table could not be read, so nothing confirms the turn's processes are \
             gone: grace: read failed; the kill of the agent's process group failed: grace: eperm"
        );
        assert!(!unchecked.contains("was still killed"));

        // Group errors alone say nothing was left running.
        assert_eq!(
            texts(&[(7, "k")], &[], &[], &["grace: eperm"]),
            (
                Some("killed 1 process(es) the turn left running: 7 (k)".to_string()),
                None
            )
        );
        assert_eq!(texts(&[], &[], &[], &["grace: eperm"]), (None, None));
    }

    /// T5: which group classifications record an error. None adds a survivor.
    #[test]
    fn only_a_failed_group_signal_is_recorded_as_a_group_error() {
        let eperm = || std::io::Error::from_raw_os_error(libc::EPERM);
        let esrch = || std::io::Error::from_raw_os_error(libc::ESRCH);
        let unknown = |signal_result| GroupKill::Unknown {
            scan_error: std::io::Error::other("scan"),
            signal_result,
        };
        let cases: Vec<(&str, GroupKill, Option<String>)> = vec![
            (
                "Failed",
                GroupKill::Failed(eperm()),
                Some(format!("s: {}", eperm())),
            ),
            (
                "Unknown, signal EPERM",
                unknown(Err(eperm())),
                Some(format!("s: {}", eperm())),
            ),
            (
                "Unknown, signal other",
                unknown(Err(std::io::Error::other("sig boom"))),
                Some("s: sig boom".to_string()),
            ),
            ("Unknown, signal ESRCH", unknown(Err(esrch())), None),
            ("Unknown, signal Ok", unknown(Ok(())), None),
            ("Killed", GroupKill::Killed(2), None),
            ("NothingAlive", GroupKill::NothingAlive, None),
        ];
        for (name, group, expected) in cases {
            let mut state = TrackerState::default();
            state.record_group_kill_error("s", &group);
            assert_eq!(
                state.group_kill_errors,
                expected.into_iter().collect::<Vec<_>>(),
                "{name}"
            );
            assert!(state.survivors.is_empty(), "{name}");
            assert!(state.survivor_starts.is_empty(), "{name}");
        }
    }

    /// T5: a Failed classification followed by a final observation with
    /// nothing alive (the macOS zombie-group `EPERM`) lists no survivor and
    /// writes no `leftovers_killed` note; the failed signal is kept.
    #[tokio::test]
    async fn a_failed_group_signal_followed_by_an_empty_check_lists_nothing() {
        let tracker = sweep_tracker(Arc::new(|_| Ok(Vec::new())));
        tracker
            .state()
            .record_group_kill_error("grace", &GroupKill::Failed(std::io::Error::other("eperm")));
        tracker.sweep(NO_AGENT, None, "grace").await;
        {
            let state = tracker.state();
            assert!(state.survivors.is_empty(), "{:?}", state.survivors);
            assert_eq!(state.group_kill_errors, vec!["grace: eperm"]);
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(killed_note, None);
        assert_eq!(unchecked, None);

        // With an earlier failed read, the failure is in the unchecked note.
        let tracker = sweep_tracker(Arc::new(|_| Ok(Vec::new())));
        tracker.note_scan_failure("cancel", &std::io::Error::other("early read failed"));
        tracker
            .state()
            .record_group_kill_error("grace", &GroupKill::Failed(std::io::Error::other("eperm")));
        tracker.sweep(NO_AGENT, None, "grace").await;
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(killed_note, None);
        assert_eq!(
            unchecked.as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: cancel: early read failed; the kill of the agent's process group \
                 failed: grace: eperm"
            )
        );
    }

    /// A process group the daemon may not signal, probed as the existing
    /// tests do. `None` (with the reason printed) when this machine has none.
    fn foreign_group() -> Option<i32> {
        // SAFETY: only reads the uid and probes pids with signal 0.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: running as root, every pid is signalable");
            return None;
        }
        let foreign = (2..5000).find(|&pid| {
            // SAFETY: signal 0 only checks permission, and the kill is a killpg.
            let rc = unsafe { libc::killpg(pid, 0) };
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        });
        if foreign.is_none() {
            eprintln!("skipped: no foreign pid found");
        }
        foreign
    }

    /// T5: with a real refused group signal, the error is recorded and the
    /// final observation alone decides whether the group is a survivor.
    #[tokio::test]
    async fn a_failed_group_signal_alone_lists_no_survivor() {
        let Some(foreign) = foreign_group() else {
            return;
        };
        let (daemon, daemon_sid) = daemon_ids();
        let row = move || sleep_entry(foreign, daemon, foreign, daemon_sid, 100);
        // Live in every table: a survivor, and the error is recorded.
        let tracker = sweep_tracker(scripted_reader(move |_| vec![row()]));
        let report = kill_and_sweep(&tracker, foreign as u32, "test").await;
        assert!(matches!(report.group, GroupKill::Failed(_)));
        {
            let state = tracker.state();
            assert_eq!(pids_of(&state.survivors), vec![foreign]);
            assert_eq!(
                state.group_kill_errors.len(),
                1,
                "{:?}",
                state.group_kill_errors
            );
            assert!(state.group_kill_errors[0].starts_with("test: "));
        }
        // Live before the signal, gone from every later table: no survivor.
        let tracker = sweep_tracker(scripted_reader(move |call| {
            if call.is_none() {
                vec![row()]
            } else {
                Vec::new()
            }
        }));
        let report = kill_and_sweep(&tracker, foreign as u32, "test").await;
        assert!(matches!(report.group, GroupKill::Failed(_)));
        {
            let state = tracker.state();
            assert!(state.survivors.is_empty(), "{:?}", state.survivors);
            assert_eq!(
                state.group_kill_errors.len(),
                1,
                "{:?}",
                state.group_kill_errors
            );
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!((killed_note, unchecked), (None, None));
    }

    /// T6: the five shapes of the `lingered` note, for each of the three
    /// reasons a grace period starts.
    #[test]
    fn the_lingered_note_says_what_was_seen_and_what_was_tried() {
        let unknown = |signal_result| GroupKill::Unknown {
            scan_error: std::io::Error::other("scan"),
            signal_result,
        };
        let cases: Vec<(&str, GroupKill, &str)> = vec![
            (
                "Killed",
                GroupKill::Killed(3),
                "3 process(es) of the agent's process group were still running 0.4s after \
                 {after}; sent SIGKILL to the group",
            ),
            (
                "Failed",
                GroupKill::Failed(std::io::Error::other("eperm boom")),
                "processes of the agent's process group were still running 0.4s after {after}; \
                 could not signal the group: eperm boom",
            ),
            (
                "Unknown, signal Ok",
                unknown(Ok(())),
                "0.4s after {after}, could not check whether the agent's process group was still \
                 running; sent SIGKILL to the group",
            ),
            (
                "Unknown, signal Err",
                unknown(Err(std::io::Error::other("sig boom"))),
                "0.4s after {after}, could not check whether the agent's process group was still \
                 running, and could not signal the group: sig boom",
            ),
            (
                "NothingAlive",
                GroupKill::NothingAlive,
                "the agent's process group had exited 0.4s after {after}, but processes it \
                 started were still running; sent them SIGKILL",
            ),
        ];
        for after in [
            "the idle reaper closed its stalled turn",
            "it was closed for never reporting",
            "its turn ended",
        ] {
            for (name, group, template) in &cases {
                assert_eq!(
                    lingered_message(group, 0.4, after),
                    template.replace("{after}", after),
                    "{name} after {after}"
                );
            }
        }
    }

    // --- the turn's outcome does not depend on the cleanup's findings ---------

    /// A turn that reports, ends its turn and exits on its own.
    fn autonomous_exit_binary(dir: &std::path::Path) -> String {
        script_binary(
            dir,
            json!([
                {"op": "read_turn"},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "exit"},
            ]),
        )
    }

    /// The grace timer must not fire in these: the process exits at once.
    fn no_grace() -> TurnTimers {
        TurnTimers {
            grace: crate::test_support::LOAD_ALLOWANCE,
            ..fast_timers(3)
        }
    }

    /// T7: a process the last check still sees does not park the turn.
    #[tokio::test]
    async fn a_survivor_changes_neither_the_status_nor_the_end_reason() {
        const GHOST: i32 = 99_999_999;
        let dir = TempDir::new();
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
            autonomous_exit_binary(&dir.0),
            no_grace(),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, None);
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_killed").await,
            Some(format!(
                "1 still running at the last check: {GHOST} (ghost)"
            ))
        );
    }

    /// T7: a sweep that cannot read the table does not park the turn either.
    #[tokio::test]
    async fn a_failed_sweep_read_changes_neither_the_status_nor_the_end_reason() {
        let dir = TempDir::new();
        let reader: crate::proc_table::ProcReader = Arc::new(|marker| {
            if marker.is_some() {
                Err(std::io::Error::other("sweep read failed"))
            } else {
                Ok(Vec::new())
            }
        });
        let (pool, session_id, _manager) = start_leftovers(
            autonomous_exit_binary(&dir.0),
            no_grace(),
            chrono::Duration::hours(1),
            Some(reader),
        )
        .await;
        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.end_reason, None);
        assert_eq!(run.status, SessionStatus::Idle);
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_killed").await,
            None
        );
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_unchecked")
                .await
                .as_deref(),
            Some(
                "the process table could not be read, so nothing confirms the turn's processes \
                 are gone: end of turn: sweep read failed"
            )
        );
    }

    // --- the pid lives until the handle is waited for -----------------------

    /// The tail signals the group before `AgentHandle::wait`, so the pid must
    /// still be the process's own until then: an exited agent whose output
    /// has drained keeps its pid, and the handle gives it up only on `wait`.
    #[tokio::test]
    async fn a_drained_exited_agent_keeps_its_pid_until_it_is_waited_for() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_oneshot.py"));
        let mut handle = adapter.start("hello", &single_shot_role_config()).unwrap();
        while handle.recv().await.is_some() {}
        let pid = handle.pid();
        assert!(
            pid.is_some(),
            "the pid is kept until the child is waited for"
        );
        // The fixture has exited: its unreaped zombie is still the pid's.
        assert!(process_alive(pid.unwrap()));
        handle.wait().await.unwrap();
        assert_eq!(
            handle.pid(),
            None,
            "the pid is gone once the child is reaped"
        );
    }

    // --- one real cleanup against `ps` ---------------------------------------

    /// One row of `ps -o pid=,ppid=,pgid=,stat=,lstart=`.
    #[derive(Debug)]
    struct PsRow {
        ppid: i32,
        pgid: i32,
        stat: String,
    }

    /// Reads `ps` output for a pid list. `ps` exits 0 when it lists at least
    /// one of the pids (the rest are simply not listed) and 1 with no output
    /// when it lists none. Those are the only readings accepted, and only with
    /// nothing on stderr. Any other status, a status 1 that printed rows, any
    /// stderr text, or a row that does not parse is an error: a failed or
    /// partial reading must not look like a process being gone.
    fn parse_ps(
        code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Result<HashMap<i32, PsRow>, String> {
        let no_match = code == Some(1) && stdout.trim().is_empty();
        if !(code == Some(0) || no_match) || !stderr.trim().is_empty() {
            return Err(format!(
                "ps failed (status {code:?}); stderr: {:?}; stdout: {stdout:?}",
                stderr.trim()
            ));
        }
        let mut rows = HashMap::new();
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let mut words = line.split_whitespace();
            let parsed = (|| {
                let pid: i32 = words.next()?.parse().ok()?;
                let ppid = words.next()?.parse().ok()?;
                let pgid = words.next()?.parse().ok()?;
                let stat = words.next()?.to_string();
                Some((pid, PsRow { ppid, pgid, stat }))
            })();
            let (pid, row) = parsed.ok_or_else(|| format!("malformed ps row: {line:?}"))?;
            rows.insert(pid, row);
        }
        Ok(rows)
    }

    /// Runs `ps` for `pids`; returns its output and the rows it listed.
    /// Panics if `ps` fails or prints a row it cannot read.
    fn ps_rows(pids: &[i32]) -> (String, HashMap<i32, PsRow>) {
        let list = pids
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let out = std::process::Command::new("ps")
            .args(["-o", "pid=,ppid=,pgid=,stat=,lstart=", "-p", &list])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let rows = parse_ps(out.status.code(), &text, &stderr).unwrap_or_else(|e| panic!("{e}"));
        (text, rows)
    }

    #[test]
    fn a_failed_or_partial_ps_reading_is_an_error_not_an_absence() {
        let row = "124 1 124 Ss Thu Nov  5 12:00:00 2026";
        // A listing, with some pids unlisted: accepted.
        let rows = parse_ps(Some(0), row, "").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&124].ppid, 1);
        assert_eq!(rows[&124].stat, "Ss");
        // Nothing matched: an empty reading, not an error.
        assert!(parse_ps(Some(1), "", "").unwrap().is_empty());
        assert!(parse_ps(Some(1), " \n", "").unwrap().is_empty());
        // Status 1 with rows is a failure that printed a partial listing, not
        // the no-match case.
        let err = parse_ps(Some(1), row, "").unwrap_err();
        assert!(err.contains("status Some(1)"), "{err}");
        // A failing command is an error even when it printed a valid row.
        let err = parse_ps(Some(2), row, "simulated-error").unwrap_err();
        assert!(err.contains("simulated-error"), "{err}");
        assert!(parse_ps(Some(2), row, "").is_err(), "status 2");
        assert!(parse_ps(None, row, "").is_err(), "killed by a signal");
        assert!(parse_ps(Some(1), row, "oops").is_err(), "stderr text");
        // A row that cannot be read is an error, not a missing process.
        let err = parse_ps(Some(0), &format!("{row}\nxx 1 2 S"), "").unwrap_err();
        assert!(err.contains("malformed ps row"), "{err}");
        assert!(parse_ps(Some(0), "123 1 123", "").is_err(), "short row");
    }

    /// SIGKILLs processes on drop, so a failed assertion leaves none behind.
    struct KillOnDrop(Vec<i32>);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            for pid in &self.0 {
                // SAFETY: only sends a signal to a pid this test started.
                unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
        }
    }

    /// Real reader, real processes, the agent test fixture (no model, no
    /// daemon): before cleanup the reader agrees with `ps`, after it the
    /// turn's processes are gone and an unrelated one is untouched.
    ///
    /// `cargo test -p chocofactoryd --lib -- --exact --ignored --nocapture
    /// session::tests::real_cleanup_agrees_with_ps`
    #[tokio::test]
    #[ignore = "real cleanup against ps; run once"]
    async fn real_cleanup_agrees_with_ps() {
        let dir = TempDir::new();
        let heartbeat = dir.0.join("heartbeat");
        let child_pid_file = dir.0.join("child.pid");
        let job_pid_file = dir.0.join("job.pid");
        let release = dir.0.join("release");
        let ready = dir.0.join("ready");
        let binary = script_binary(
            &dir.0,
            json!([
                {"op": "read_turn"},
                {"op": "spawn_child", "heartbeat": heartbeat, "pid_file": child_pid_file},
                {
                    "op": "spawn_escaped", "setsid": true, "double_fork": true,
                    "wait_orphaned": true, "pid_file": job_pid_file,
                },
                // `spawn_escaped` returns only once the intermediate process
                // has exited, so the job is reparented: this file says the
                // fixture is there. Then it is held, so the "before"
                // readings are taken while everything it started is alive
                // and settled.
                {"op": "run", "command": format!("touch '{}'", ready.display())},
                {"op": "wait_file", "path": release},
                {"op": "report", "outcome": "done"},
                {"op": "result"},
                {"op": "sleep", "seconds": 60},
            ]),
        );
        let timers = TurnTimers {
            grace: crate::test_support::RESPONSE_MARGIN,
            ..fast_timers(3)
        };
        let mut unrelated = Sleeper::spawn(None, true);
        let (pool, session_id, manager) =
            start_leftovers(binary, timers, chrono::Duration::hours(1), None).await;
        let child = read_pid_when_written(&child_pid_file).await as i32;
        let job = read_pid_when_written(&job_pid_file).await as i32;
        let _cleanup = KillOnDrop(vec![child, job]);
        crate::test_support::wait_until("the same-group heartbeat", || async {
            match std::fs::metadata(&heartbeat) {
                Ok(meta) if meta.len() > 0 => Ok(()),
                other => Err(format!("{other:?}")),
            }
        })
        .await;
        crate::test_support::wait_until("the fixture to reach its barrier", || async {
            if ready.exists() {
                Ok(())
            } else {
                Err("not yet".to_string())
            }
        })
        .await;
        let leader = {
            let sessions = manager.sessions.lock().await;
            let Some(SessionSlot::Live(session)) = sessions.get(&session_id) else {
                panic!("session should be live");
            };
            let pgid = *session.signals.pgid.lock().await;
            pgid.expect("a live session has a pgid") as i32
        };
        let pids = [leader, child, job, unrelated.pid()];

        let table = proc_table::read(None).unwrap();
        let (ps_before, rows_before) = ps_rows(&pids);
        println!("ps before cleanup:\n{ps_before}");
        for (name, pid) in ["leader", "same-group child", "escaped job", "unrelated"]
            .iter()
            .zip(pids)
        {
            let entry = table
                .iter()
                .find(|e| e.pid == pid)
                .unwrap_or_else(|| panic!("the reader lists the {name} ({pid})"));
            let row = rows_before
                .get(&pid)
                .unwrap_or_else(|| panic!("ps lists the {name} ({pid})"));
            println!(
                "{name} {pid}: reader ppid {} pgid {} start {}; ps ppid {} pgid {} stat {}",
                entry.ppid, entry.pgid, entry.start, row.ppid, row.pgid, row.stat
            );
            assert_eq!(entry.ppid, row.ppid, "{name} ppid");
            assert_eq!(entry.pgid, row.pgid, "{name} pgid");
        }
        let job_comm = table.iter().find(|e| e.pid == job).unwrap().comm.clone();

        let released = std::time::Instant::now();
        std::fs::write(&release, "").unwrap();
        let run = wait_until_final(&pool, &session_id).await;
        println!(
            "release to final status: {:?} (end_reason {:?})",
            released.elapsed(),
            run.end_reason
        );
        for note in note_events(&pool, &session_id).await {
            println!("note {}: {}", note.payload["kind"], note.payload["message"]);
        }
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_killed").await,
            Some(format!(
                "killed 1 process(es) the turn left running: {job} ({job_comm})"
            ))
        );
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_unchecked").await,
            None
        );

        // The turn's processes end up absent or zombies awaiting a reaper.
        let gone = |rows: &HashMap<i32, PsRow>, pid: i32| {
            rows.get(&pid).is_none_or(|row| row.stat.starts_with('Z'))
        };
        let (ps_after, rows_after) =
            crate::test_support::wait_until("the turn's processes to be gone", || async {
                let (text, rows) = ps_rows(&pids);
                if [leader, child, job].iter().all(|pid| gone(&rows, *pid)) {
                    Ok((text, rows))
                } else {
                    Err(text)
                }
            })
            .await;
        println!("ps after cleanup:\n{ps_after}");
        let unrelated_row = rows_after.get(&unrelated.pid());
        assert!(
            unrelated_row.is_some_and(|row| !row.stat.starts_with('Z')),
            "the unrelated sleeper is still running: {unrelated_row:?}"
        );
        assert!(unrelated.is_running());
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
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
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
            role: "coder".to_string(),
            project_id: None,
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

    // --- diagnostics apart from grace policy ---------------------------------

    /// A table row the marker owns, outside the agent's group, with its own
    /// command so the tests can tell which scan a name came from.
    fn escapee_row(pid: i32, start: u64, comm: &str) -> crate::proc_table::ProcEntry {
        let mut row = marked(sleep_entry(pid, 1, pid, pid, start));
        row.comm = comm.into();
        row
    }

    fn named_leader(agent: i32) -> crate::proc_table::ProcEntry {
        let mut row = leader_row(agent, agent);
        row.comm = "leader".into();
        row
    }

    /// A row no evidence ties to the turn: its own session and group, no
    /// marker, parent 1.
    fn bystander_row(pid: i32) -> crate::proc_table::ProcEntry {
        let mut row = sleep_entry(pid, 1, pid, pid, 900);
        row.comm = "bystander".into();
        row
    }

    /// The count the production sweep put on its last-round warning, if it
    /// logged one.
    fn last_check_count(log: &str) -> Option<usize> {
        let line = log
            .lines()
            .find(|l| l.contains("still running at the sweep's last check"))?;
        let digits: String = line
            .split("count=")
            .nth(1)?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    }

    /// One tracker, three sites, in the order the daemon runs them for a
    /// completed turn that is shut down: an earlier site, grace, the tail.
    ///
    /// The earlier site finds an owned job only in a round past its original
    /// stop point, so it signals it for the diagnostics without a policy
    /// count. The job is still there at grace, which finds it before its own
    /// stop point: that is the count the grace decision reads. The table
    /// rows after a SIGKILL are scripted visibility, not survival: the real
    /// process is killed, and left unreaped so a second delivery succeeds.
    #[tokio::test]
    async fn cross_site_late_delivery_does_not_suppress_grace_policy() {
        // Ordinary earlier delivery: counted once, and a later site signals
        // the same pid again (redelivery verified through the refreshed
        // identity) without counting it again.
        let mut ordinary = Sleeper::spawn(None, true);
        let f = ordinary.pid();
        let (reader, phase) = phased_reader(move |phase, scan| {
            Ok(match (phase, scan) {
                (0, 0) => vec![escapee_row(f, 700, "ordinary-first")],
                (1, 0) => vec![escapee_row(f, 701, "ordinary-second")],
                _ => Vec::new(),
            })
        });
        let tracker = sweep_tracker(reader);
        let earlier = tracker.sweep(NO_AGENT, None, "cancel").await;
        assert_eq!(earlier, 1);
        phase.next();
        let grace = tracker.sweep(NO_AGENT, None, "grace").await;
        {
            let state = tracker.state();
            assert_eq!(
                (state.killed.clone(), state.killed_starts.clone()),
                (
                    vec![(f, "ordinary-second".to_string())],
                    HashMap::from([(f, 701)])
                ),
                "the second signal was delivered, not an ESRCH"
            );
        }
        assert_eq!(grace, 0, "an ordinary earlier delivery already counted");
        assert_eq!(ordinary.reap_signal(), Some(libc::SIGKILL));

        // Late discovery: one tracker, the sites in the daemon's order.
        let (_, daemon_sid) = daemon_ids();
        let mut escapee = Sleeper::spawn(None, true);
        let e = escapee.pid();
        let mut group_member = Sleeper::spawn(None, false);
        let g = group_member.pid();
        let mut bystander = Sleeper::spawn(None, true);
        let b = bystander.pid();
        let (reader, phase) = phased_reader(move |phase, scan| {
            let mut table = vec![bystander_row(b)];
            match (phase, scan) {
                // Earlier site: the leader, then the job past the stop point.
                (0, 0) => table.push(leader_row(NO_AGENT, NO_AGENT)),
                (0, 1) => {
                    table.push(leader_row(NO_AGENT, NO_AGENT));
                    table.push(escapee_row(e, 400, "escapee-late"));
                }
                // Grace: the job before its stop point, and an owned member
                // of the group whose kill was delivered (watched, spared).
                (1, 0..=1) => {
                    if scan == 0 {
                        table.push(escapee_row(e, 555, "escapee-grace"));
                    }
                    table.push(marked(sleep_entry(g, 1, g, daemon_sid, 300)));
                }
                // The tail finds it once more.
                (2, 0) => table.push(escapee_row(e, 556, "escapee-tail")),
                _ => {}
            }
            Ok(table)
        });
        let tracker = sweep_tracker(reader);

        let earlier = tracker.sweep(NO_AGENT, None, "shutdown").await;
        {
            let state = tracker.state();
            assert_eq!(earlier, 0, "a late delivery is not a policy count");
            assert!(state.policy_counted_pids.is_empty());
            assert_eq!(state.killed, vec![(e, "escapee-late".to_string())]);
            assert_eq!(state.killed_starts, HashMap::from([(e, 400)]));
        }
        phase.next();
        let grace = tracker.sweep(NO_AGENT, Some(g), "grace").await;
        {
            let state = tracker.state();
            assert_eq!(grace, 1, "the late discovery must not use up the count");
            assert_eq!(
                state.policy_counted_pids,
                std::collections::HashSet::from([e])
            );
            assert_eq!(
                state.killed,
                vec![(e, "escapee-grace".to_string())],
                "listed once, with the latest command"
            );
            assert_eq!(state.killed_starts, HashMap::from([(e, 555)]));
        }
        assert!(lingered_decision(&GroupKill::NothingAlive, grace));
        phase.next();
        let tail = tracker.sweep(NO_AGENT, None, "end of turn").await;
        assert_eq!(tail, 0, "the tail does not count the pid again");
        assert_eq!(tracker.state().killed_starts, HashMap::from([(e, 556)]));
        assert!(
            lingered_decision(&GroupKill::NothingAlive, grace),
            "the grace decision was made before the tail and stays"
        );
        assert_eq!(escapee.reap_signal(), Some(libc::SIGKILL));
        assert!(
            group_member.is_running(),
            "a spared group member is not signalled"
        );
        assert!(
            bystander.is_running(),
            "an unrelated process is never signalled"
        );
        assert!(
            !pids_of(&tracker.state().killed).contains(&g)
                && !pids_of(&tracker.state().killed).contains(&b)
        );
    }

    /// A completed main-agent turn, queued whole: the report call, its
    /// successful result and the end of the turn.
    fn queue_completed_report(events_tx: &mpsc::UnboundedSender<AgentEvent>) {
        use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};
        let report_tool = chocofactory_core::mcp::qualified_report_outcome_tool_name();
        let tokens = TokenCounts {
            input: Some(120),
            output: Some(45),
            cache_read: Some(900),
            cache_write: Some(30),
        };
        for event in [
            AgentEvent::ToolCall {
                tool_use_id: "toolu_report".into(),
                tool: report_tool.clone(),
                input: json!({"outcome": "done", "report": "## Summary\nImplemented the change and ran the tests."}),
            },
            AgentEvent::ToolResult {
                tool_use_id: "toolu_report".into(),
                tool: report_tool,
                output: "reported: done".into(),
                is_error: false,
            },
            AgentEvent::TurnCompleted {
                is_error: false,
                usage: TurnUsage {
                    cost_usd: Some(0.0123),
                    tokens,
                    models: None,
                    wall_time_ms: Some(4200),
                    model_turns: Some(3),
                    billing: BillingMode::Subscription,
                    counting: UsageCounting::PerTurn,
                },
            },
        ] {
            events_tx.send(event).unwrap();
        }
    }

    /// The whole drain: a completed turn, shut down. The shutdown-style
    /// cleanup finds an owned job only after its original stop point, and
    /// grace carries that delivery. Controlled tables keep the same identity
    /// visible through grace and the tail: scripted visibility after real
    /// SIGKILL delivery, not OS survival. Neither note may call it killed.
    #[tokio::test]
    async fn shutdown_then_grace_preserves_completed_run_status() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
        let mut escapee = Sleeper::spawn(None, true);
        let e = escapee.pid();
        let mut bystander = Sleeper::spawn(None, true);
        let b = bystander.pid();

        let mut command = tokio::process::Command::new("sleep");
        command.arg("300").process_group(0).kill_on_drop(true);
        let child = command.spawn().unwrap();
        let leader = child.id().unwrap() as i32;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (stdin_tx, _stdin_rx) = mpsc::unbounded_channel();
        let marker = crate::adapter::new_turn_marker().unwrap();
        let handle = AgentHandle::new(child, events_rx, stdin_tx, marker);

        // Every sweep uses all its rounds while the same job stays visible.
        // The unmarked pre-kill scans see no live group member.
        let marker_scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&marker_scans);
        let reader: crate::proc_table::ProcReader = Arc::new(move |marker| {
            if marker.is_none() {
                return Ok(Vec::new());
            }
            let mut table = vec![bystander_row(b)];
            let scan = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if scan <= 1 {
                table.push(leader_row(leader, leader));
            }
            if scan > 0 {
                table.push(escapee_row(e, 400, "escapee"));
            }
            Ok(table)
        });
        let tracker = Arc::new(LeftoverTracker::new(
            handle.marker().to_string(),
            SessionKind::SingleShot,
            reader,
        ));
        let signals = SessionSignals {
            last_activity: Arc::new(Mutex::new(Utc::now())),
            pgid: Arc::new(Mutex::new(handle.pid())),
            cancelled: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(true)),
            tracker: Arc::clone(&tracker),
        };
        let pgid = Arc::clone(&signals.pgid);

        // The report call, its successful result and the end of the turn are
        // queued before the drain starts; the sender stays open.
        queue_completed_report(&events_tx);

        // The shutdown-style cleanup owns the group lock before the drain
        // can take it for the turn's end.
        let held = pgid.lock().await;
        let drain = tokio::spawn({
            let (pool, session_id) = (pool.clone(), session_id.clone());
            async move {
                let timers = TurnTimers {
                    grace: StdDuration::from_millis(20),
                    ..fast_timers(0)
                };
                drain_session(
                    &pool,
                    &session_id,
                    handle,
                    SessionKind::SingleShot,
                    mpsc::unbounded_channel().1,
                    signals,
                    chrono::Duration::hours(1),
                    &timers,
                    &Notify::new(),
                )
                .await;
            }
        });
        let earlier = kill_and_sweep(&tracker, leader as u32, "shutdown").await;
        drop(held);
        assert!(
            matches!(earlier.group, GroupKill::NothingAlive),
            "{:?}",
            earlier.group
        );
        assert_eq!(earlier.sweep_killed, 0, "found only past the stop point");

        // Hold the event stream through grace's final observation, after the
        // earlier sweep has already used all its rounds.
        crate::test_support::wait_until("grace's final scan", || async {
            let n = marker_scans.load(std::sync::atomic::Ordering::SeqCst);
            if n >= 2 * (SWEEP_ROUNDS + 1) {
                Ok(())
            } else {
                Err(format!("{n} marker scans so far"))
            }
        })
        .await;
        drop(events_tx);
        drain.await.unwrap();

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(
            (run.status, run.end_reason),
            (SessionStatus::Exited, Some(SessionEndReason::Lingered)),
            "cross-site late discovery must preserve the original persisted status and reason"
        );
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_killed").await,
            Some(format!("1 still running at the last check: {e} (escapee)")),
            "the carried identity is still observed at the tail's last check"
        );
        let lingered = note_message(&pool, &session_id, "lingered").await.unwrap();
        assert!(
            lingered.contains("an earlier cleanup of this turn sent SIGKILL to a process")
                && !lingered.contains("killed"),
            "a carry-only note claims SIGKILL delivery, never death: {lingered}"
        );
        assert!(
            lingered.contains("the agent's process group had exited"),
            "grace's pre-kill check saw nothing alive: {lingered}"
        );
        // Grace signalled the escapee again, but the earlier site's count
        // already covers it, so grace's own count is zero. The note must not
        // claim grace found or signalled nothing.
        assert!(
            !lingered.contains("nothing to signal") && !lingered.contains("found nothing"),
            "a deduplicated count of zero says nothing about what grace signalled: {lingered}"
        );
        assert_eq!(escapee.reap_signal(), Some(libc::SIGKILL));
        assert!(
            bystander.is_running(),
            "an unrelated process is never signalled"
        );
        assert_eq!(
            *pgid.lock().await,
            None,
            "the group was retired before the reap"
        );
        // SAFETY: signal 0 only checks that the pid exists.
        assert_eq!(
            unsafe { libc::kill(leader, 0) },
            -1,
            "the leader was reaped"
        );
        assert!(tracker.state().policy_counted_pids.is_empty());
    }

    /// The state of `pid` as `ps` reports it ("" when there is no such
    /// process): an independent view of a process the tests script into a
    /// table.
    fn ps_state(pid: i32) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// One tracker through the sites in the daemon's order, with the carry
    /// the grace site takes on entry. Which kills count at grace:
    /// - `o`: counted by the first site's ordinary round, signalled again in
    ///   its extra rounds: already counted, not carried.
    /// - `l1`, `l2`: signalled only in the first site's extra rounds. `l2`
    ///   has exited by grace; `l1` is found again by grace before its own
    ///   stop point. Each is carried and counts once.
    /// - `m`: signalled in the first site's extra rounds, then counted by a
    ///   second site's ordinary round: counted already, not carried.
    /// - `g`: found by grace after its own stop point; `t`: found only by the
    ///   tail. Neither is carried, and neither counts.
    ///
    /// Every signal is real; the rows are scripted visibility.
    #[tokio::test]
    async fn earlier_late_kills_count_once_at_grace_and_only_those() {
        let a = NO_AGENT;
        let mut o = Sleeper::spawn(None, true);
        let mut l1 = Sleeper::spawn(None, true);
        let mut l2 = Sleeper::spawn(None, true);
        let mut m = Sleeper::spawn(None, true);
        let mut g = Sleeper::spawn(None, true);
        let mut t = Sleeper::spawn(None, true);
        let mut bystander = Sleeper::spawn(None, true);
        let (po, p1, p2, pm, pg, pt, pb) = (
            o.pid(),
            l1.pid(),
            l2.pid(),
            m.pid(),
            g.pid(),
            t.pid(),
            bystander.pid(),
        );
        let (reader, phase) = phased_reader(move |phase, scan| {
            let mut table = vec![bystander_row(pb)];
            match (phase, scan) {
                // First site: `o` before its stop point; the leader alone
                // marks the stop point; then the extra rounds see `o` again
                // and find `l1`, `l2` and `m`.
                (0, 0) => table.push(escapee_row(po, 600, "o")),
                (0, 1) | (2, 1) | (3, 0) => table.push(leader_row(a, a)),
                (0, 2) => {
                    table.push(escapee_row(po, 600, "o-again"));
                    table.push(escapee_row(p1, 610, "l1-late"));
                    table.push(escapee_row(p2, 620, "l2-late"));
                    table.push(escapee_row(pm, 650, "m-late"));
                }
                // `l1` is still there next round: signalled a second time.
                (0, 3) => table.push(escapee_row(p1, 612, "l1-late-again")),
                // Second site: `m`, before its stop point.
                (1, 0) => table.push(escapee_row(pm, 651, "m-ordinary")),
                // Grace: `l1` before its stop point; `g` after it.
                (2, 0) => table.push(escapee_row(p1, 611, "l1-grace")),
                (2, 2) => table.push(escapee_row(pg, 630, "g-late")),
                // The tail: `t`, after its stop point.
                (3, 1) => table.push(escapee_row(pt, 640, "t-late")),
                _ => {}
            }
            Ok(table)
        });
        let tracker = sweep_tracker(reader);

        let first = tracker.sweep(a, None, "shutdown").await;
        assert_eq!(first, 1, "only `o` was found before the stop point");
        {
            let state = tracker.state();
            assert_eq!(
                state.killed_starts.get(&p1),
                Some(&612),
                "`l1` was signalled a second time in the extra rounds"
            );
            assert_eq!(
                state.late_killed_pids,
                vec![p1, p2, pm],
                "`o` was already counted; `l1`, signalled twice, is listed once"
            );
        }
        assert_eq!(
            l2.reap_signal(),
            Some(libc::SIGKILL),
            "`l2` is gone by grace"
        );
        phase.next();
        assert_eq!(tracker.sweep(a, None, "cancel").await, 1, "`m`, once");
        phase.next();
        let carried = tracker.state().carry_late_kills();
        assert_eq!(carried, 2, "`l1` and `l2` are carried; `o` and `m` are not");
        let grace = tracker.sweep(a, None, "grace").await;
        assert_eq!(grace, 0, "`l1` is deduplicated against the carried count");
        {
            let state = tracker.state();
            assert_eq!(
                state.killed_starts.get(&p1),
                Some(&611),
                "`l1` was signalled again at grace"
            );
            assert_eq!(
                state.policy_counted_pids,
                std::collections::HashSet::from([po, p1, p2, pm])
            );
        }
        phase.next();
        assert_eq!(tracker.sweep(a, None, "end of turn").await, 0);
        assert_eq!(
            tracker.state().policy_counted_pids,
            std::collections::HashSet::from([po, p1, p2, pm]),
            "neither grace's own extra-round find nor the tail's is counted"
        );
        for sleeper in [&mut o, &mut l1, &mut m, &mut g, &mut t] {
            assert_eq!(sleeper.reap_signal(), Some(libc::SIGKILL));
        }
        assert!(
            bystander.is_running(),
            "an unrelated process is never signalled"
        );
    }

    /// The whole drain, as `shutdown_then_grace_preserves_completed_run_status`
    /// runs it, with a worker the earlier site kills in its extra rounds and
    /// that has really exited by grace, which finds nothing. The earlier kill
    /// cannot erase the lingering result by succeeding: the grace kill is still
    /// recorded in a `lingered` note, and the verified cleanup then keeps the
    /// completed turn complete.
    #[tokio::test]
    async fn an_earlier_kill_that_really_died_still_counts_as_lingering_but_a_verified_cleanup_completes()
     {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session_for(&pool, SessionKind::SingleShot).await;
        let mut worker = Sleeper::spawn(None, true);
        let w = worker.pid();
        let mut bystander = Sleeper::spawn(None, true);
        let b = bystander.pid();

        let mut command = tokio::process::Command::new("sleep");
        command.arg("300").process_group(0).kill_on_drop(true);
        let child = command.spawn().unwrap();
        let leader = child.id().unwrap() as i32;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (stdin_tx, _stdin_rx) = mpsc::unbounded_channel();
        let marker = crate::adapter::new_turn_marker().unwrap();
        let handle = AgentHandle::new(child, events_rx, stdin_tx, marker);

        // Marker scans, in order: 0-2 the earlier site, 3 grace, 4 the tail.
        // The worker is in scan 1 only, and `ps` is asked about it there.
        let marker_scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&marker_scans);
        let ps_when_shown = Arc::new(std::sync::Mutex::new(String::new()));
        let shown = Arc::clone(&ps_when_shown);
        let reader: crate::proc_table::ProcReader = Arc::new(move |marker| {
            if marker.is_none() {
                return Ok(Vec::new());
            }
            let mut table = vec![bystander_row(b)];
            match seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => table.push(leader_row(leader, leader)),
                1 => {
                    *shown.lock().unwrap() = ps_state(w);
                    table.push(leader_row(leader, leader));
                    table.push(escapee_row(w, 400, "worker"));
                }
                _ => {}
            }
            Ok(table)
        });
        let tracker = Arc::new(LeftoverTracker::new(
            handle.marker().to_string(),
            SessionKind::SingleShot,
            reader,
        ));
        let signals = SessionSignals {
            last_activity: Arc::new(Mutex::new(Utc::now())),
            pgid: Arc::new(Mutex::new(handle.pid())),
            cancelled: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(true)),
            tracker: Arc::clone(&tracker),
        };
        let pgid = Arc::clone(&signals.pgid);
        queue_completed_report(&events_tx);

        let held = pgid.lock().await;
        let drain = tokio::spawn({
            let (pool, session_id) = (pool.clone(), session_id.clone());
            async move {
                let timers = TurnTimers {
                    grace: StdDuration::from_millis(20),
                    ..fast_timers(0)
                };
                drain_session(
                    &pool,
                    &session_id,
                    handle,
                    SessionKind::SingleShot,
                    mpsc::unbounded_channel().1,
                    signals,
                    chrono::Duration::hours(1),
                    &timers,
                    &Notify::new(),
                )
                .await;
            }
        });
        let earlier = kill_and_sweep(&tracker, leader as u32, "shutdown").await;
        assert_eq!(earlier.sweep_killed, 0, "found only past the stop point");
        // The worker was alive and not a zombie when shown; it is reaped, so
        // really gone, before grace can take the group lock.
        let shown_as = ps_when_shown.lock().unwrap().clone();
        assert!(
            !shown_as.is_empty() && !shown_as.starts_with('Z'),
            "the worker was alive when the table showed it: {shown_as:?}"
        );
        assert_eq!(worker.reap_signal(), Some(libc::SIGKILL));
        assert_eq!(ps_state(w), "", "the worker is gone before grace");
        drop(held);

        crate::test_support::wait_until("grace's final scan", || async {
            let n = marker_scans.load(std::sync::atomic::Ordering::SeqCst);
            if n >= 4 {
                Ok(())
            } else {
                Err(format!("{n} marker scans so far"))
            }
        })
        .await;
        drop(events_tx);
        drain.await.unwrap();

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(
            (run.status, run.end_reason, run.cleanup),
            (SessionStatus::Idle, None, Some(SessionCleanup::Verified)),
            "the completed turn stays complete after a verified cleanup"
        );
        // The note exists only if grace counted the earlier kill as lingering.
        let lingered = note_message(&pool, &session_id, "lingered").await.unwrap();
        assert!(
            lingered.contains("an earlier cleanup of this turn sent SIGKILL to a process")
                && !lingered.contains("killed")
                && !lingered.contains("still running")
                && !lingered.contains("found nothing"),
            "the note names only the earlier signal delivery: {lingered}"
        );
        assert_eq!(
            note_message(&pool, &session_id, "leftovers_killed").await,
            Some(format!(
                "killed 1 process(es) the turn left running: {w} (worker)"
            ))
        );
        assert!(
            bystander.is_running(),
            "an unrelated process is never signalled"
        );
        assert_eq!(
            *pgid.lock().await,
            None,
            "the group was retired before the reap"
        );
        // SAFETY: signal 0 only checks that the pid exists.
        assert_eq!(
            unsafe { libc::kill(leader, 0) },
            -1,
            "the leader was reaped"
        );
    }

    /// The last-round warning counts what the survivor list and the note
    /// report: here the leader and a signalled job whose marker is gone, so
    /// only the signal's identity keeps it in the final set. The job is
    /// really signalled; its rows after the signal are scripted visibility,
    /// not survival.
    #[tokio::test]
    async fn final_warning_counts_signalled_identity_after_marker_loss() {
        let a = NO_AGENT;
        let mut escapee = Sleeper::spawn(None, true);
        let e = escapee.pid();
        let mut bystander = Sleeper::spawn(None, true);
        let b = bystander.pid();
        let leader = move |_: ()| named_leader(a);
        let reader = scripted_reader(move |call| match call {
            // The marker shows in the first scan only.
            Some(1) => vec![leader(()), escapee_row(e, 400, "escapee"), bystander_row(b)],
            _ => {
                let mut row = sleep_entry(e, 1, e, e, 400);
                row.comm = "escapee".into();
                vec![leader(()), row, bystander_row(b)]
            }
        });
        let tracker = sweep_tracker(reader);
        let (log, guard) = capture_logs();
        tracker.sweep(a, None, "grace").await;
        let log_text = log.text();
        drop(guard);
        assert_eq!(
            last_check_count(&log_text),
            Some(2),
            "final warning must count the leader and the live signalled identity: {log_text}"
        );
        {
            let state = tracker.state();
            assert_eq!(sorted(pids_of(&state.survivors)), vec![e, a]);
            assert_eq!(state.survivor_starts, HashMap::from([(e, 400), (a, 100)]));
            assert_paired(&state);
        }
        let (killed_note, unchecked) = written_notes(&tracker).await;
        assert_eq!(
            killed_note,
            Some(format!(
                "2 still running at the last check: {e} (escapee), {a} (leader)"
            ))
        );
        assert_eq!(unchecked, None);
        assert_eq!(escapee.reap_signal(), Some(libc::SIGKILL));
        assert!(bystander.is_running());

        // The same job named by all three sources at once (marker, group,
        // signal) is one process, counted once.
        let mut both = Sleeper::spawn(None, true);
        let o = both.pid();
        let reader = scripted_reader(move |_| {
            let mut row = marked(sleep_entry(o, 1, a, o, 400));
            row.comm = "overlap".into();
            vec![leader(()), row, bystander_row(b)]
        });
        let tracker = sweep_tracker(reader);
        let (log, guard) = capture_logs();
        tracker.sweep(a, None, "grace").await;
        let log_text = log.text();
        drop(guard);
        assert_eq!(last_check_count(&log_text), Some(2), "{log_text}");
        assert_eq!(sorted(pids_of(&tracker.state().survivors)), vec![o, a]);
        assert_eq!(both.reap_signal(), Some(libc::SIGKILL));
        assert!(bystander.is_running());
    }
}
