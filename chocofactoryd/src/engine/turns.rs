//! Capping how many single-shot agent turns run at once, and queueing the
//! rest.
//!
//! [`TurnGate::claim`] is the one place that decides whether a turn starts
//! now or waits. It runs under the task's lock (the caller holds it) and then
//! under the gate's slot lock, always in that order; nothing holds the slot
//! lock while waiting for a task lock. "Count, decide, take" is one critical
//! section under the slot lock: every permit is issued under it and every
//! `turn_queue` write is made under it. A permit is *released* outside the
//! lock (on drop); a release only lowers the count, so it can make a decision
//! cautious, never over the limit, and it always wakes the dispatcher.
//!
//! A turn that starts holds a [`TurnPermit`] with exactly one owner at a time:
//! `launch_turn` until the process is spawned, then the turn watcher until
//! the process is gone. The queued turns are rows in `turn_queue`; the
//! dispatcher (`run_dispatcher`) starts them in order as slots free.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Duration;

use chocofactory_core::models::{
    AgentPool, QueuedTurn, ServerQueuedTurn,
};

use super::parallel::BranchEntry;
use super::turn::ResumeSession;
use super::*;
use crate::db::turn_queue::{self, NewQueueRow, QueueRow};

/// How often the dispatcher looks at the queue even when nothing woke it, so
/// an edited `max_agents` applies without a restart.
pub(crate) const DEFAULT_DISPATCH_TICK: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The admissible predicate and the withdraw-or-keep rule
// ---------------------------------------------------------------------------

/// The Rust half of the admissible predicate (the SQL half is
/// `turn_queue::ADMISSIBLE_SQL`): a queued row counts toward positions and
/// may be started only while its task is `open`.
pub(crate) fn is_admissible(status: &str) -> bool {
    status == TASK_STATUS_OPEN
}

/// What to do with a queue row, judged by its task's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowFate {
    /// The task is `open`: the row may start when its turn comes.
    Admissible,
    /// The row can never start (its task is gone, stuck, closed or
    /// cancelled): delete it.
    Withdraw,
    /// The task is in a status this code does not know: leave the row where
    /// it is.
    Keep,
}

/// The one place a queue decision depends on a task's status. Task status is
/// a string, so the compiler cannot force every status to be classified; a
/// new status lands in the last arm and keeps its rows until it is classified
/// here.
pub(crate) fn queue_row_fate(status: Option<&str>) -> RowFate {
    match status {
        None => RowFate::Withdraw,
        Some(status) if is_admissible(status) => RowFate::Admissible,
        Some(TASK_STATUS_STUCK | TASK_STATUS_CLOSED | TASK_STATUS_CANCELLED) => RowFate::Withdraw,
        Some(other) => {
            tracing::warn!(
                status = other,
                "a queued turn's task has a status the queue does not know; keeping its row"
            );
            RowFate::Keep
        }
    }
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

/// A set of turns that share a limit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PoolKey {
    /// Every project without a cap of its own.
    Server,
    /// One project with its own cap.
    Project(String),
}

impl PoolKey {
    /// The wire name: `server` or `project:<id>`.
    pub(crate) fn wire(&self) -> String {
        match self {
            PoolKey::Server => "server".to_string(),
            PoolKey::Project(id) => format!("project:{id}"),
        }
    }
}

/// A task's pool, from its project's *current* cap.
pub(crate) fn pool_of(project_id: &str, caps: &HashMap<String, u32>) -> PoolKey {
    if caps.contains_key(project_id) {
        PoolKey::Project(project_id.to_string())
    } else {
        PoolKey::Server
    }
}

fn limit_of(pool: &PoolKey, caps: &HashMap<String, u32>, server: Option<u32>) -> Option<u32> {
    match pool {
        PoolKey::Server => server,
        PoolKey::Project(id) => caps.get(id).copied(),
    }
}

/// An admissible queue row with its pool and its 1-based place in it.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    pub(crate) row: QueueRow,
    pub(crate) pool: PoolKey,
    pub(crate) position: u32,
}

/// The admissible rows of `rows` (already in queue order), each with its pool
/// and its place among the admissible rows of that pool. Rows that are not
/// admissible have no place and are left out.
pub(crate) fn place(rows: Vec<QueueRow>, caps: &HashMap<String, u32>) -> Vec<Placed> {
    let mut counts: HashMap<PoolKey, u32> = HashMap::new();
    let mut placed = Vec::new();
    for row in rows {
        if !is_admissible(&row.task_status) {
            continue;
        }
        let pool = pool_of(&row.project_id, caps);
        let count = counts.entry(pool.clone()).or_insert(0);
        *count += 1;
        placed.push(Placed {
            position: *count,
            pool,
            row,
        });
    }
    placed
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PermitInfo {
    task_id: String,
    project_id: String,
}

/// A slot held by one running single-shot turn. Dropping it frees the slot
/// (it cannot be dropped twice or skipped) and wakes the dispatcher.
#[derive(Debug)]
pub(crate) struct TurnPermit {
    gate: Arc<TurnGate>,
    id: u64,
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        self.gate
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
        self.gate.wake.notify_one();
    }
}

/// What a turn is, as far as the queue is concerned.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnSpec<'a> {
    /// The agent stage, or the branch's name.
    pub(crate) stage: &'a str,
    /// The parallel group and its entry number, for a branch.
    pub(crate) group: Option<(&'a str, i64)>,
    /// The session a retry chose to resume.
    pub(crate) resume_session_id: Option<&'a str>,
}

/// Where a queued turn stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedAt {
    pub(crate) position: u32,
}

#[derive(Debug)]
pub(crate) enum Claim {
    /// A slot was free: the turn starts now, holding this permit.
    Start(TurnPermit),
    /// The turn waits in the queue.
    Queued(QueuedAt),
}

/// The slot accounting and the queue's decision point.
#[derive(Debug)]
pub struct TurnGate {
    pool: SqlitePool,
    config_path: Option<PathBuf>,
    /// Serializes every permit and every `turn_queue` write. Taken after the
    /// task lock, never while waiting for one.
    slot: Mutex<()>,
    running: std::sync::Mutex<HashMap<u64, PermitInfo>>,
    next_permit: AtomicU64,
    /// Wakes the dispatcher: a slot freed, a turn queued, a cap changed.
    wake: Notify,
    stopping: AtomicBool,
    queue_error: std::sync::Mutex<Option<String>>,
    tick_ms: AtomicU64,
}

impl TurnGate {
    pub(crate) fn new(pool: SqlitePool, config_path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            config_path,
            slot: Mutex::new(()),
            running: std::sync::Mutex::new(HashMap::new()),
            next_permit: AtomicU64::new(0),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
            queue_error: std::sync::Mutex::new(None),
            tick_ms: AtomicU64::new(DEFAULT_DISPATCH_TICK.as_millis() as u64),
        })
    }

    /// Shortens the dispatcher's tick (tests).
    #[cfg(test)]
    pub(crate) fn set_tick(&self, tick: Duration) {
        self.tick_ms
            .store(tick.as_millis() as u64, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn tick(&self) -> Duration {
        Duration::from_millis(self.tick_ms.load(Ordering::SeqCst))
    }

    /// The daemon is stopping: from now on a turn due to start is queued, and
    /// the dispatcher starts nothing and exits.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// The slot lock, for a writer of `turn_queue` outside the gate.
    pub(crate) async fn slot_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.slot.lock().await
    }

    /// Wakes the dispatcher (a cap changed, for instance).
    pub fn wake_dispatcher(&self) {
        self.wake.notify_one();
    }

    /// The agent turns of `task_id` that are running now.
    pub(crate) fn running_for_task(&self, task_id: &str) -> usize {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|info| info.task_id == task_id)
            .count()
    }

    fn running_in(&self, pool: &PoolKey, caps: &HashMap<String, u32>) -> u32 {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|info| &pool_of(&info.project_id, caps) == pool)
            .count() as u32
    }

    #[cfg(test)]
    fn running_total(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn issue(self: &Arc<Self>, task_id: &str, project_id: &str) -> TurnPermit {
        let id = self.next_permit.fetch_add(1, Ordering::SeqCst);
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id,
                PermitInfo {
                    task_id: task_id.to_string(),
                    project_id: project_id.to_string(),
                },
            );
        TurnPermit {
            gate: Arc::clone(self),
            id,
        }
    }

    fn load_config(&self) -> Result<GlobalConfig, GlobalConfigError> {
        match &self.config_path {
            Some(path) => GlobalConfig::load(path),
            None => Ok(GlobalConfig::default()),
        }
    }

    fn set_queue_error(&self, error: Option<String>) {
        *self
            .queue_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
    }

    pub(crate) fn queue_error(&self) -> Option<String> {
        self.queue_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Decides whether the turn described by `spec`, of task `task_id`
    /// (whose lock the caller holds), starts now or queues.
    ///
    /// An unreadable or malformed `config.yaml`, or a failed queue write, is
    /// an `Err`: the caller fails the turn the way a spawn failure fails it.
    /// There is no "start anyway" and no silent default.
    pub(crate) async fn claim(
        self: &Arc<Self>,
        task_id: &str,
        spec: &TurnSpec<'_>,
    ) -> Result<Claim, EngineError> {
        let (claim, queued_event) = {
            let _slot = self.slot.lock().await;
            let task = tasks::get(&self.pool, task_id)
                .await?
                .ok_or(EngineError::NoSuchTask)?;
            let config = self.load_config().map_err(EngineError::GlobalConfig)?;
            let caps = projects::caps(&self.pool).await?;
            let pool = pool_of(&task.project_id, &caps);
            let limit = limit_of(&pool, &caps, config.max_agents);
            let placed = place(turn_queue::list(&self.pool).await?, &caps);
            let ahead = placed.iter().filter(|p| p.pool == pool).count() as u32;
            let running = self.running_in(&pool, &caps);
            let starts = !self.stopping()
                && match limit {
                    None => true,
                    Some(limit) => ahead == 0 && running < limit,
                };
            if starts {
                (Claim::Start(self.issue(task_id, &task.project_id)), None)
            } else {
                turn_queue::insert(
                    &self.pool,
                    NewQueueRow {
                        task_id,
                        stage: spec.stage,
                        group_stage: spec.group.map(|(group, _)| group),
                        group_entry: spec.group.map(|(_, entry)| entry),
                        resume_session_id: spec.resume_session_id,
                    },
                )
                .await?;
                let position = ahead + 1;
                let event = json!({
                    "stage": spec.stage,
                    "group": spec.group.map(|(group, _)| group),
                    "entry": spec.group.map(|(_, entry)| entry),
                    "pool": pool.wire(),
                    "limit": limit,
                    "running": running,
                    "position": position,
                });
                (Claim::Queued(QueuedAt { position }), Some(event))
            }
        };
        if let Some(payload) = queued_event {
            // After the commit it describes, best-effort like every
            // timeline write; the state is in the row.
            if let Err(err) =
                events::append_for_task(&self.pool, task_id, EventType::TurnQueued, payload).await
            {
                tracing::error!(task_id, %err, "failed to record a turn_queued event");
            }
            self.wake.notify_one();
        }
        Ok(claim)
    }

    /// One look at the queue under the slot lock. With `withdraw`, first
    /// deletes every row that can never start (compare-and-set on the status
    /// it was judged by). Then finds the earliest admissible row that is the
    /// head of its pool and whose pool has a free slot; with `issue_for`, it
    /// only counts when it is that row, and the permit is issued in the same
    /// critical section.
    async fn pick(
        self: &Arc<Self>,
        issue_for: Option<i64>,
        withdraw: bool,
    ) -> Result<Picked, PickError> {
        let _slot = self.slot.lock().await;
        let mut picked = Picked::default();
        if self.stopping() {
            return Ok(picked);
        }
        let rows = turn_queue::list(&self.pool).await.map_err(PickError::Db)?;
        if rows.is_empty() {
            self.set_queue_error(None);
            return Ok(picked);
        }
        // Fail closed: a config that cannot be read starts nothing. The
        // queue is left alone and the error is shown on `GET /server`.
        let config = match self.load_config() {
            Ok(config) => {
                self.set_queue_error(None);
                config
            }
            Err(err) => {
                self.set_queue_error(Some(err.to_string()));
                return Err(PickError::Config(err.to_string()));
            }
        };
        if withdraw {
            for row in &rows {
                if queue_row_fate(Some(&row.task_status)) != RowFate::Withdraw {
                    continue;
                }
                match turn_queue::delete_if_task_status(&self.pool, row.id, &row.task_status).await
                {
                    Ok(true) => picked.withdrawn.push(Withdrawn {
                        task_id: row.task_id.clone(),
                        stage: row.stage.clone(),
                        group: row.group_stage.clone(),
                        reason: format!("the task is {}", row.task_status),
                    }),
                    Ok(false) => {}
                    Err(err) => {
                        tracing::error!(
                            task_id = %row.task_id, stage = %row.stage, %err,
                            "failed to withdraw a queued turn that can never start"
                        );
                    }
                }
            }
        }
        let caps = projects::caps(&self.pool).await.map_err(PickError::Db)?;
        let placed = place(rows, &caps);
        let candidate = placed
            .into_iter()
            .filter(|p| p.position == 1)
            .filter(|p| match limit_of(&p.pool, &caps, config.max_agents) {
                None => true,
                Some(limit) => self.running_in(&p.pool, &caps) < limit,
            })
            .min_by_key(|p| p.row.id);
        if let Some(candidate) = candidate {
            if let Some(expected) = issue_for {
                if candidate.row.id == expected {
                    picked.permit = Some(self.issue(&candidate.row.task_id, &candidate.row.project_id));
                    picked.row = Some(candidate.row);
                }
            } else {
                picked.row = Some(candidate.row);
            }
        }
        Ok(picked)
    }
}

/// A queue row dropped without starting.
#[derive(Debug)]
struct Withdrawn {
    task_id: String,
    stage: String,
    group: Option<String>,
    reason: String,
}

#[derive(Default)]
struct Picked {
    withdrawn: Vec<Withdrawn>,
    row: Option<QueueRow>,
    permit: Option<TurnPermit>,
}

enum PickError {
    /// `config.yaml` cannot be read; carries the error.
    Config(String),
    Db(sqlx::Error),
}

/// What the dispatcher did in one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Something changed; look again at once.
    Progress,
    /// Nothing can start now; wait for a wake or the tick.
    Idle,
}

/// Why a picked row was not started: it can never start, so it is deleted
/// with this reason. (A row whose task has a status the queue does not know
/// is kept by `queue_row_fate` before validation is reached.)
enum Refusal {
    Withdraw(String),
}

// ---------------------------------------------------------------------------
// What the API shows
// ---------------------------------------------------------------------------

/// The queue and the pools as of one read.
#[derive(Debug, Clone, Default)]
pub struct QueueSnapshot {
    pub pools: Vec<AgentPool>,
    /// Every waiting turn, in queue order: `(task id, title, turn, group
    /// entry)`.
    pub turns: Vec<ServerQueuedTurn>,
    entries: Vec<Option<i64>>,
    pub queue_error: Option<String>,
}

impl QueueSnapshot {
    /// The waiting turns of one task, in queue order.
    pub fn queued_for(&self, task_id: &str) -> Vec<QueuedTurn> {
        self.turns
            .iter()
            .filter(|t| t.task_id == task_id)
            .map(|t| t.turn.clone())
            .collect()
    }

    /// The place of the waiting turn `(task, stage)` of `group`, if it is
    /// waiting.
    pub fn position_of(&self, task_id: &str, stage: &str, group: Option<&str>) -> Option<u32> {
        self.turns
            .iter()
            .find(|t| {
                t.task_id == task_id && t.turn.stage == stage && t.turn.group.as_deref() == group
            })
            .map(|t| t.turn.position)
    }

    /// Whether `(task, stage)` of `group` waits in the queue for group entry
    /// `entry` (any entry when `None`).
    pub fn is_queued(
        &self,
        task_id: &str,
        stage: &str,
        group: Option<&str>,
        entry: Option<i64>,
    ) -> bool {
        self.turns
            .iter()
            .zip(&self.entries)
            .any(|(t, e)| {
                t.task_id == task_id
                    && t.turn.stage == stage
                    && t.turn.group.as_deref() == group
                    && (entry.is_none() || *e == entry)
            })
    }
}

impl WorkflowEngine {
    /// Reads the pools and the queue. A `config.yaml` that cannot be read
    /// leaves the server pool's limit `None` and reports the error in
    /// `queue_error`: it is never shown as a healthy "no limit".
    pub async fn queue_snapshot(&self) -> Result<QueueSnapshot, sqlx::Error> {
        let gate = &self.turn_gate;
        let (server_limit, config_error) = match gate.load_config() {
            Ok(config) => (config.max_agents, None),
            Err(err) => (None, Some(err.to_string())),
        };
        let caps = projects::caps(&self.pool).await?;
        let placed = place(turn_queue::list(&self.pool).await?, &caps);
        let all_projects = projects::list(&self.pool).await?;

        let mut pools = vec![AgentPool {
            pool: PoolKey::Server.wire(),
            name: "server".to_string(),
            limit: server_limit,
            running: gate.running_in(&PoolKey::Server, &caps),
            queued: placed.iter().filter(|p| p.pool == PoolKey::Server).count() as u32,
        }];
        let mut capped: Vec<_> = all_projects
            .iter()
            .filter_map(|p| p.max_agents.map(|limit| (p, limit)))
            .collect();
        capped.sort_by(|a, b| a.0.name.cmp(&b.0.name).then(a.0.id.cmp(&b.0.id)));
        for (project, limit) in capped {
            let key = PoolKey::Project(project.id.clone());
            pools.push(AgentPool {
                pool: key.wire(),
                name: project.name.clone(),
                limit: Some(limit),
                running: gate.running_in(&key, &caps),
                queued: placed.iter().filter(|p| p.pool == key).count() as u32,
            });
        }
        let mut turns = Vec::with_capacity(placed.len());
        let mut entries = Vec::with_capacity(placed.len());
        for p in placed {
            let wire = p.pool.wire();
            let pool = pools.iter().find(|pool| pool.pool == wire);
            turns.push(ServerQueuedTurn {
                task_id: p.row.task_id.clone(),
                title: p.row.title.clone(),
                turn: QueuedTurn {
                    stage: p.row.stage.clone(),
                    group: p.row.group_stage.clone(),
                    position: p.position,
                    pool: wire,
                    pool_name: pool.map(|pool| pool.name.clone()).unwrap_or_default(),
                    limit: pool.and_then(|pool| pool.limit),
                    running: pool.map(|pool| pool.running).unwrap_or(0),
                    queued_at: p.row.queued_at,
                },
            });
            entries.push(p.row.group_entry);
        }
        Ok(QueueSnapshot {
            pools,
            turns,
            entries,
            queue_error: gate.queue_error().or(config_error),
        })
    }

    /// The sweep's "is this task's current work held?" hook: a top-level
    /// agent stage with a queue row is waiting for a slot, not running, and
    /// a restart does not strand it.
    pub(super) async fn current_work_held(
        &self,
        task_id: &str,
        current_stage: &str,
    ) -> Result<bool, sqlx::Error> {
        turn_queue::exists(&self.pool, task_id, current_stage, None).await
    }

    /// Wakes the dispatcher: a project's cap changed.
    pub fn wake_dispatcher(&self) {
        self.turn_gate.wake_dispatcher();
    }

    /// The gate, for the daemon's shutdown order and tests.
    pub fn turn_gate(&self) -> &Arc<TurnGate> {
        &self.turn_gate
    }

    /// Deletes every queue row of `task_id` under the slot lock and writes
    /// one `turn_withdrawn` event per row. The caller holds the task lock.
    /// Used by a retry before it re-enters anything: the rows of a stuck task
    /// can never start, and a leftover one would collide with the re-claim.
    pub(super) async fn withdraw_task_rows(
        &self,
        task_id: &str,
        reason: &str,
    ) -> Result<usize, sqlx::Error> {
        let removed = {
            let _slot = self.turn_gate.slot.lock().await;
            turn_queue::delete_for_task(&self.pool, task_id).await?
        };
        for (stage, group) in &removed {
            turn_queue::record_withdrawn(&self.pool, task_id, stage, group.as_deref(), reason)
                .await;
        }
        if !removed.is_empty() {
            self.events_notify.notify_waiters();
        }
        Ok(removed.len())
    }

    /// Starts the dispatcher: the one task that starts queued turns. It
    /// sleeps on a wake and a tick, and exits once the gate is stopped.
    pub fn start_dispatcher(self: &Arc<Self>) -> JoinHandle<()> {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if engine.turn_gate.stopping() {
                    return;
                }
                while engine.dispatch_pass().await == Pass::Progress {
                    if engine.turn_gate.stopping() {
                        return;
                    }
                }
                let gate = &engine.turn_gate;
                tokio::select! {
                    () = gate.wake.notified() => {}
                    () = tokio::time::sleep(gate.tick()) => {}
                }
            }
        })
    }

    /// One dispatcher pass: start at most one queued turn.
    ///
    /// 1. Under the slot lock, withdraw the rows that can never start and find
    ///    the earliest admissible row that heads its pool and has a free slot.
    /// 2. Take that task's lock, then the slot lock again, and check it is
    ///    still that row; issue the permit in the same critical section.
    /// 3. Validate the row against the task's current state.
    /// 4. Rebuild the turn and launch it: the row's deletion and the session
    ///    insert are one transaction.
    /// 5. On a prepare or launch failure, fail the stage or branch as a
    ///    detached failure is failed, then delete the row.
    async fn dispatch_pass(self: &Arc<Self>) -> Pass {
        let candidate = match self.turn_gate.pick(None, true).await {
            Ok(picked) => {
                self.record_withdrawn(&picked.withdrawn).await;
                match picked.row {
                    Some(row) => row,
                    None => {
                        return if picked.withdrawn.is_empty() {
                            Pass::Idle
                        } else {
                            Pass::Progress
                        };
                    }
                }
            }
            Err(PickError::Config(err)) => {
                tracing::error!(%err, "the queue cannot start turns: config.yaml is unusable");
                return Pass::Idle;
            }
            Err(PickError::Db(err)) => {
                tracing::error!(%err, "the queue could not be read");
                return Pass::Idle;
            }
        };

        let lock = self.lock_for_task(&candidate.task_id).await;
        let pass = {
            let _guard = lock.lock().await;
            match self.turn_gate.pick(Some(candidate.id), false).await {
                Ok(Picked {
                    row: Some(row),
                    permit: Some(permit),
                    ..
                }) => self.admit(row, permit).await,
                Ok(_) => Pass::Progress,
                Err(_) => Pass::Idle,
            }
        };
        self.evict_task_lock_if_unshared(&candidate.task_id, &lock)
            .await;
        pass
    }

    async fn record_withdrawn(&self, withdrawn: &[Withdrawn]) {
        for w in withdrawn {
            turn_queue::record_withdrawn(
                &self.pool,
                &w.task_id,
                &w.stage,
                w.group.as_deref(),
                &w.reason,
            )
            .await;
        }
        if !withdrawn.is_empty() {
            self.events_notify.notify_waiters();
        }
    }

    /// Deletes a row the dispatcher judged can never start, with its event.
    /// Under the task lock the caller holds, and the slot lock.
    async fn withdraw_row(&self, row: &QueueRow, reason: &str) -> Pass {
        let deleted = {
            let _slot = self.turn_gate.slot.lock().await;
            turn_queue::delete(&self.pool, row.id).await
        };
        match deleted {
            Ok(true) => {
                turn_queue::record_withdrawn(
                    &self.pool,
                    &row.task_id,
                    &row.stage,
                    row.group_stage.as_deref(),
                    reason,
                )
                .await;
                self.events_notify.notify_waiters();
                Pass::Progress
            }
            Ok(false) => Pass::Progress,
            Err(err) => {
                tracing::error!(
                    task_id = %row.task_id, stage = %row.stage, %err,
                    "failed to withdraw a queued turn that can never start; retrying next tick"
                );
                Pass::Idle
            }
        }
    }

    /// Steps 3-5 of the pass, under the task lock, holding `permit`.
    async fn admit(self: &Arc<Self>, row: QueueRow, permit: TurnPermit) -> Pass {
        let task_id = row.task_id.as_str();
        let stage = row.stage.as_str();
        macro_rules! read {
            ($e:expr) => {
                match $e {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::error!(task_id, stage, %err, "could not read the state of a queued turn; it stays queued");
                        return Pass::Idle;
                    }
                }
            };
        }

        // Step 3: validate.
        let task = read!(tasks::get(&self.pool, task_id).await);
        match queue_row_fate(task.as_ref().map(|t| t.status.as_str())) {
            RowFate::Admissible => {}
            RowFate::Withdraw => {
                let why = match &task {
                    Some(task) => format!("the task is {}", task.status),
                    None => "the task no longer exists".to_string(),
                };
                drop(permit);
                return self.withdraw_row(&row, &why).await;
            }
            RowFate::Keep => return Pass::Idle,
        }
        let Some(task) = task else {
            return Pass::Idle;
        };
        let state = read!(workflow_state::get(&self.pool, task_id).await);
        let Some(state) = state else {
            drop(permit);
            return self
                .withdraw_row(&row, "the task has no workflow state")
                .await;
        };
        let definition = match self.load_task_workflow(&task).await {
            Ok(definition) => Arc::new(definition),
            Err(err) => {
                let err = EngineError::QueueAdmission(format!(
                    "the task's workflow could not be loaded: {err}"
                ));
                return self.fail_admission(&row, None, err, permit).await;
            }
        };
        match self.validate_row(&row, &state, &definition).await {
            Ok(None) => {}
            Ok(Some(Refusal::Withdraw(why))) => {
                drop(permit);
                return self.withdraw_row(&row, &why).await;
            }
            Err(err) => {
                tracing::error!(task_id, stage, %err, "could not validate a queued turn; it stays queued");
                return Pass::Idle;
            }
        }

        // Step 4: rebuild the turn from the row and the current payload.
        let resume = match self.resume_for_row(&row).await {
            Ok(resume) => resume,
            Err(err) => return self.fail_admission(&row, Some(&definition), err, permit).await,
        };
        let input = state
            .payload
            .get("task")
            .and_then(|task| task.get("input"))
            .and_then(Value::as_str);
        let group_def = row.group_stage.as_deref().and_then(|group| {
            match definition.stages.get(group).map(|d| &d.kind) {
                Some(StageKind::Parallel { branches }) => branches.get(stage),
                _ => None,
            }
        });
        let (stage_def, branch_entry) = match (&row.group_stage, group_def) {
            (None, _) => match definition.stages.get(stage) {
                Some(def) => (def, None),
                None => {
                    drop(permit);
                    return self
                        .withdraw_row(&row, "the stage is no longer in the workflow")
                        .await;
                }
            },
            (Some(group), Some(branch)) => (
                &branch.def,
                Some(BranchEntry {
                    group: group.as_str(),
                    entry: row.group_entry.unwrap_or(0),
                    results: &branch.results,
                }),
            ),
            (Some(_), None) => {
                drop(permit);
                return self
                    .withdraw_row(&row, "the branch is no longer in the workflow")
                    .await;
            }
        };
        let entry = StageEntry {
            task_id,
            definition: &definition,
            stage_name: stage,
            stage_def,
            payload: &state.payload,
            input,
            resume: resume.as_ref(),
            branch: branch_entry,
        };
        let prepared = match self.prepare_turn(&entry).await {
            Ok(prepared) => prepared,
            Err(err) => return self.fail_admission(&row, Some(&definition), err, permit).await,
        };
        match self.launch_turn(prepared, Some(permit), Some(row.id)).await {
            Ok(session_id) => {
                let waited_ms = (Utc::now() - row.queued_at).num_milliseconds().max(0);
                if let Err(err) = events::append_for_task(
                    &self.pool,
                    task_id,
                    EventType::TurnDequeued,
                    json!({
                        "stage": stage,
                        "group": row.group_stage,
                        "entry": row.group_entry,
                        "waited_ms": waited_ms,
                        "session_id": session_id,
                    }),
                )
                .await
                {
                    tracing::error!(task_id, stage, %err, "failed to record a turn_dequeued event");
                } else {
                    self.events_notify.notify_waiters();
                }
                Pass::Progress
            }
            // The row went away (cancel, withdrawal) while the turn was being
            // prepared: nothing started, nothing to fail. The permit was
            // dropped inside `launch_turn`.
            Err(EngineError::QueueRowVanished(_)) => Pass::Progress,
            // `launch_turn` owned the permit and dropped it; the row is
            // still there to clean up.
            Err(err) => self.fail_admission_after_launch(&row, &definition, err).await,
        }
    }

    /// The row against the task's current state. `None`: it may start.
    async fn validate_row(
        &self,
        row: &QueueRow,
        state: &chocofactory_core::models::WorkflowState,
        definition: &WorkflowDefinition,
    ) -> Result<Option<Refusal>, sqlx::Error> {
        let moved = |why: String| Ok(Some(Refusal::Withdraw(why)));
        match row.group_stage.as_deref() {
            None => {
                if state.current_stage != row.stage {
                    return moved(format!(
                        "the task has moved on to '{}'",
                        state.current_stage
                    ));
                }
                if !matches!(
                    definition.stages.get(&row.stage).map(|d| &d.kind),
                    Some(StageKind::AgentTurn { .. })
                ) {
                    return moved(format!("'{}' is not an agent_turn stage", row.stage));
                }
            }
            Some(group) => {
                if state.current_stage != group {
                    return moved(format!(
                        "the task has moved on from '{group}' to '{}'",
                        state.current_stage
                    ));
                }
                let block = state
                    .payload
                    .get("parallel")
                    .filter(|b| b.get("stage").and_then(Value::as_str) == Some(group));
                let entry = block.and_then(|b| b.get("entry")).and_then(Value::as_i64);
                if entry != row.group_entry {
                    return moved(format!("'{group}' has been entered again since"));
                }
                let slot_state = block
                    .and_then(|b| b.get("branches"))
                    .and_then(|b| b.get(&row.stage))
                    .and_then(|s| s.get("state"))
                    .and_then(Value::as_str);
                if slot_state != Some("running") {
                    return moved(format!("branch '{}' has already settled", row.stage));
                }
                if let Some(session) =
                    sessions::get_current_for_stage(&self.pool, &row.task_id, &row.stage).await?
                    && session.status == SessionStatus::Active
                {
                    return moved(format!("branch '{}' already has a live session", row.stage));
                }
            }
        }
        Ok(None)
    }

    /// Rebuilds the `ResumeSession` a queued `retry --resume` recorded, from
    /// that session's row.
    async fn resume_for_row(&self, row: &QueueRow) -> Result<Option<ResumeSession>, EngineError> {
        let Some(previous) = row.resume_session_id.as_deref() else {
            return Ok(None);
        };
        let unusable = |why: &str| {
            EngineError::QueueAdmission(format!(
                "the session {previous} that this queued turn was to resume {why}"
            ))
        };
        let session = sessions::get(&self.pool, previous)
            .await?
            .ok_or_else(|| unusable("no longer exists"))?;
        Ok(Some(ResumeSession {
            adapter_session_id: session
                .adapter_session_id
                .ok_or_else(|| unusable("has no adapter session id"))?,
            cli_adapter: session.cli_adapter,
            previous_session_id: session.id,
            end_reason: session
                .end_reason
                .ok_or_else(|| unusable("has no end reason"))?,
        }))
    }

    /// A failure before `launch_turn` took the permit.
    async fn fail_admission(
        self: &Arc<Self>,
        row: &QueueRow,
        definition: Option<&Arc<WorkflowDefinition>>,
        err: EngineError,
        permit: TurnPermit,
    ) -> Pass {
        let pass = self.fail_admission_inner(row, definition, err).await;
        drop(permit);
        pass
    }

    async fn fail_admission_after_launch(
        self: &Arc<Self>,
        row: &QueueRow,
        definition: &Arc<WorkflowDefinition>,
        err: EngineError,
    ) -> Pass {
        self.fail_admission_inner(row, Some(definition), err).await
    }

    /// Handles a prepare or launch failure as a detached failure is handled
    /// today (`mark_stuck` for a stage, `record_start_failures` for a
    /// branch), then deletes the row. When that delete fails the pass goes
    /// idle until the next tick instead of starting the same turn again at
    /// once.
    async fn fail_admission_inner(
        self: &Arc<Self>,
        row: &QueueRow,
        definition: Option<&Arc<WorkflowDefinition>>,
        err: EngineError,
    ) -> Pass {
        let task_id = row.task_id.as_str();
        let stage = row.stage.as_str();
        tracing::error!(task_id, stage, %err, "a queued turn failed to start");
        let branches = row.group_stage.as_deref().and_then(|group| {
            match definition?.stages.get(group).map(|d| &d.kind) {
                Some(StageKind::Parallel { branches }) => Some((group, branches)),
                _ => None,
            }
        });
        match (&row.group_stage, branches) {
            (None, _) => {
                self.mark_stuck(
                    task_id,
                    &format!("stage '{stage}': failed to start after waiting in the queue: {err}"),
                    false,
                )
                .await;
            }
            (Some(_), Some((group, branches))) => {
                if let Err(record_err) = self
                    .record_start_failures(
                        task_id,
                        group,
                        branches,
                        row.group_entry.unwrap_or(0),
                        &[(stage.to_string(), err.to_string())],
                    )
                    .await
                {
                    self.mark_stuck(
                        task_id,
                        &format!(
                            "stage '{}': branch '{stage}' failed to start ({err}) and the \
                             failure could not be recorded: {record_err}",
                            row.group_stage.as_deref().unwrap_or_default()
                        ),
                        false,
                    )
                    .await;
                }
            }
            (Some(group), None) => {
                self.mark_stuck(
                    task_id,
                    &format!(
                        "stage '{group}': branch '{stage}' failed to start after waiting in the \
                         queue: {err}"
                    ),
                    false,
                )
                .await;
            }
        }
        self.events_notify.notify_waiters();
        let deleted = {
            let _slot = self.turn_gate.slot.lock().await;
            turn_queue::delete(&self.pool, row.id).await
        };
        match deleted {
            Ok(_) => Pass::Progress,
            Err(delete_err) => {
                tracing::error!(
                    task_id, stage, %delete_err,
                    "failed to delete the queue row of a turn that failed to start; the next \
                     pass withdraws it when its task is no longer open"
                );
                Pass::Idle
            }
        }
    }

    /// How many permits are held right now (tests).
    #[cfg(test)]
    pub(crate) fn running_total(&self) -> usize {
        self.turn_gate.running_total()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_row_fate_classifies_every_known_status_and_keeps_the_unknown() {
        assert_eq!(queue_row_fate(None), RowFate::Withdraw);
        assert_eq!(queue_row_fate(Some("open")), RowFate::Admissible);
        assert_eq!(queue_row_fate(Some("stuck")), RowFate::Withdraw);
        assert_eq!(queue_row_fate(Some("closed")), RowFate::Withdraw);
        assert_eq!(queue_row_fate(Some("cancelled")), RowFate::Withdraw);
        assert_eq!(queue_row_fate(Some("some_new_status")), RowFate::Keep);
    }

    #[test]
    fn only_open_is_admissible() {
        assert!(is_admissible("open"));
        for status in ["stuck", "closed", "cancelled", "other", ""] {
            assert!(!is_admissible(status), "{status}");
        }
    }
}
