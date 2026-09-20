use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chocofactory_core::models::{EventType, SessionEndReason, SessionStatus};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::adapter::{AdapterError, AgentAdapter, AgentEvent, AgentHandle, RoleConfig};
use crate::db::{events, sessions};

/// Drives the active ⇄ idle ⇄ resume state machine (§4.1) on top of
/// `sessions`: keeps a live `AgentHandle` per active `session_id`,
/// drains its events into the `events` table, and resumes a fresh
/// process from the persisted `adapter_session_id` when a message arrives for a
/// run that isn't currently live in memory.
pub struct SessionManager {
    pool: SqlitePool,
    adapter: Arc<dyn AgentAdapter>,
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
        Self::with_turn_timers(
            pool,
            adapter,
            idle_timeout,
            events_notify,
            TurnTimers::default(),
        )
    }

    /// [`Self::new`] with non-default [`TurnTimers`], so tests can exercise
    /// nudges and the grace kill in milliseconds rather than minutes.
    pub fn with_turn_timers(
        pool: SqlitePool,
        adapter: Arc<dyn AgentAdapter>,
        idle_timeout: chrono::Duration,
        events_notify: Arc<Notify>,
        turn_timers: TurnTimers,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            adapter,
            idle_timeout,
            turn_timers,
            sessions: Mutex::new(HashMap::new()),
            events_notify,
        })
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
        prompt: &str,
        cfg: &RoleConfig,
        kind: SessionKind,
    ) -> Result<(), SessionError> {
        self.reserve(session_id).await?;

        let handle = match self.adapter.start(prompt, cfg) {
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
        adapter_session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
        kind: SessionKind,
    ) -> Result<(), SessionError> {
        self.reserve(session_id).await?;

        let handle = match self.adapter.resume(adapter_session_id, prompt, cfg) {
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

        // Re-checked atomically here (rather than trusting the read
        // above): two concurrent calls for the same not-yet-live
        // session_id can both reach this point, but only one of them
        // wins the reservation. The loser reports AlreadyStarting instead
        // of also resuming, which would otherwise spawn a duplicate
        // process and corrupt this map (§ review on PR #28).
        self.reserve(session_id).await?;

        let handle = match self.adapter.resume(&adapter_session_id, text, cfg) {
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
            // Read before `handle` moves into the drain task below — that
            // task owns it exclusively from then on, and `cancel` needs the
            // pgid without being able to reach the handle. Cleared again by
            // `drain_session` the moment the child is about to be reaped.
            pgid: Arc::new(Mutex::new(handle.pid())),
        };

        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionSlot::Live(ActiveSession {
                cmd_tx,
                signals: signals.clone(),
            }),
        );

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
///    applies, and the run ends `NoReport`.
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
                if let AgentEvent::SessionMeta { session_id: adapter_session_id, .. } = &event
                    && let Err(err) = sessions::set_adapter_session_id(pool, session_id, adapter_session_id).await
                {
                    tracing::error!(session_id, %err, "failed to persist adapter_session_id");
                }
                let event_type = event.event_type();
                let mut payload = event.payload();
                if turn.completed
                    && let Value::Object(map) = &mut payload
                {
                    map.insert("after_completion".to_string(), Value::Bool(true));
                }
                match events::append(pool, session_id, event_type, payload).await {
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
                let killed = match *pgid.lock().await {
                    Some(pgid) => {
                        crate::shell::kill_group(pgid);
                        true
                    }
                    None => false,
                };
                if killed {
                    turn.lingered = true;
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
                        "process still running after stdin was closed; killed its process group"
                    );
                    append_session_note(
                        pool,
                        session_id,
                        "lingered",
                        &format!(
                            "the agent process was still running {:.1}s after {after}; killed its \
                             process group",
                            turn_timers.grace.as_secs_f32()
                        ),
                        events_notify,
                    )
                    .await;
                } else {
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
            crate::shell::kill_group(group);
        }
        *pgid = None;
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
}

impl Default for TurnTimers {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            nudge_after: Duration::from_secs(5 * 60),
            max_nudges: 3,
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
        }
    }
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
            AgentEvent::TurnCompleted { is_error: true } => {
                self.errored = true;
                self.waiting_for_report = false;
                TurnStep::EndedWithError
            }
            AgentEvent::TurnCompleted { is_error: false } if self.reported => {
                self.completed = true;
                self.waiting_for_report = false;
                TurnStep::Completed
            }
            AgentEvent::TurnCompleted { is_error: false } => {
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
        self.waiting_for_report && !self.gave_up && self.grace_deadline.is_none()
    }
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
) -> (SessionStatus, Option<SessionEndReason>) {
    use SessionEndReason::{Cancelled, Interrupted, Lingered, NoReport, Reaped};
    use SessionStatus::{Exited, Idle};
    match kind {
        // A clean exit (reaper-driven close, or a one-shot process finishing on
        // its own) goes to `idle`, ready to resume. A crash, auth failure, or
        // signal kill goes to `exited` instead — otherwise a deterministic
        // failure would just get resumed into the same crash forever.
        SessionKind::Standing => {
            let status = if clean_exit { Idle } else { Exited };
            let reason = if cancelled {
                Some(Cancelled)
            } else if clean_exit && reaped {
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
    use crate::adapter::ClaudeAdapter;
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
        for _ in 0..200 {
            let stored = events::list_for_session(pool, session_id).await.unwrap();
            if stored.len() >= expected {
                return stored;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for {expected} events");
    }

    async fn wait_until_status(pool: &SqlitePool, session_id: &str, expected: SessionStatus) {
        for _ in 0..200 {
            let run = sessions::get(pool, session_id).await.unwrap().unwrap();
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
        let session_id = seed_session(&pool).await;
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
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
            .await
            .unwrap();

        let stored = wait_until_events_len(&pool, &session_id, 2).await;
        assert_eq!(stored[1].payload["text"], "echo:hello");

        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(run.status, SessionStatus::Active);
        assert!(run.adapter_session_id.is_some());
    }

    #[tokio::test]
    async fn send_message_forwards_to_an_active_in_memory_session() {
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py")));
        let manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
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
            adapter,
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
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(
                &session_id,
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
                "#!/bin/sh\nCHOCO_TEST_HEARTBEAT='{}' CHOCO_TEST_CHILD_PID='{}' exec '{}' \"$@\"\n",
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
        for _ in 0..1000 {
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "go", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "go", &role_config(), SessionKind::Standing)
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
            adapter,
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
            adapter,
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
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
        );

        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
        );

        // Grab the shared pgid handle while the session is live, so it can
        // still be inspected after the map slot is gone.
        manager
            .start(&session_id, "hello", &role_config(), SessionKind::Standing)
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
            adapter,
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
            timers,
        );
        manager
            .start(
                &session_id,
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
        for _ in 0..500 {
            let run = sessions::get(pool, session_id).await.unwrap().unwrap();
            if run.status != SessionStatus::Active {
                return run;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for the run to leave active");
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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(2)).await;

        let run = wait_until_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited);
        assert_eq!(run.end_reason, Some(SessionEndReason::NoReport));
        assert_eq!(
            session_notes(&pool, &session_id).await,
            vec!["nudge", "nudge", "no_report"]
        );
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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(0)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(0)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(0)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;

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
        let (pool, session_id, _manager) = start_single_shot(binary, fast_timers(3)).await;

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
            adapter,
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
            .start(&session_id, "hello", &cfg, SessionKind::Standing)
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
        use SessionEndReason::{Cancelled, Interrupted, Lingered, NoReport, Reaped};
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
                expected: (Idle, None),
            },
            Case {
                name: "completed, non-zero exit",
                kind: SingleShot,
                turn: completed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Idle, None),
            },
            Case {
                name: "completed then killed",
                kind: SingleShot,
                turn: lingered_after_completion(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Exited, Some(Lingered)),
            },
            Case {
                name: "never reported, killed",
                kind: SingleShot,
                turn: gave_up_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "never reported, exited on its own",
                kind: SingleShot,
                turn: silent(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "crashed",
                kind: SingleShot,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Exited, None),
            },
            Case {
                name: "errored result, clean exit",
                kind: SingleShot,
                turn: errored(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                expected: (Exited, None),
            },
            Case {
                name: "errored result, killed",
                kind: SingleShot,
                turn: errored_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Exited, Some(Lingered)),
            },
            Case {
                name: "reaped, clean exit",
                kind: SingleShot,
                turn: silent(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                expected: (Idle, Some(Reaped)),
            },
            Case {
                name: "reaped, then killed",
                kind: SingleShot,
                turn: killed(),
                clean_exit: false,
                reaped: true,
                cancelled: false,
                expected: (Exited, Some(Reaped)),
            },
            Case {
                name: "reaped after giving up",
                kind: SingleShot,
                turn: gave_up(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "cancelled while waiting",
                kind: SingleShot,
                turn: waiting(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats lingered",
                kind: SingleShot,
                turn: lingered_after_completion(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats no report",
                kind: SingleShot,
                turn: gave_up_and_killed(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled beats reaped",
                kind: SingleShot,
                turn: silent(),
                clean_exit: false,
                reaped: true,
                cancelled: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "cancelled after completing cleanly",
                kind: SingleShot,
                turn: completed(),
                clean_exit: true,
                reaped: false,
                cancelled: true,
                expected: (Idle, Some(Cancelled)),
            },
            Case {
                name: "interrupted by a usage limit",
                kind: SingleShot,
                turn: interrupted(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
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
                expected: (Exited, Some(NoReport)),
            },
            Case {
                name: "cancelled beats interrupted",
                kind: SingleShot,
                turn: interrupted(),
                clean_exit: false,
                reaped: false,
                cancelled: true,
                expected: (Exited, Some(Cancelled)),
            },
            Case {
                name: "standing, clean exit",
                kind: Standing,
                turn: silent(),
                clean_exit: true,
                reaped: false,
                cancelled: false,
                expected: (Idle, None),
            },
            Case {
                name: "standing, reaped",
                kind: Standing,
                turn: silent(),
                clean_exit: true,
                reaped: true,
                cancelled: false,
                expected: (Idle, Some(Reaped)),
            },
            Case {
                name: "standing, crashed",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: false,
                cancelled: false,
                expected: (Exited, None),
            },
            Case {
                name: "standing, cancelled",
                kind: Standing,
                turn: silent(),
                clean_exit: false,
                reaped: true,
                cancelled: true,
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
                    case.cancelled
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
            adapter,
            chrono::Duration::zero(),
            Arc::new(Notify::new()),
            fast_timers(3),
        );
        manager
            .start(
                &session_id,
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
                {"op": "sleep", "seconds": 1.0},
                {"op": "report", "outcome": "done"},
                {"op": "text", "text": "done"},
                {"op": "result"},
            ]),
        );
        // A pause comfortably longer than the nudge window, deliberately: a
        // turn wrongly treated as waiting would be nudged twice here, so the
        // empty note list below is the assertion that matters.
        let timers = TurnTimers {
            nudge_after: StdDuration::from_millis(400),
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
            metas[0].payload["session_id"],
            metas[1].payload["session_id"]
        );
        assert_eq!(
            run.adapter_session_id.as_deref(),
            metas[0].payload["session_id"].as_str()
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
}
