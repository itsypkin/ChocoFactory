//! Workflow engine (design §5): the generic stage/transition interpreter
//! that drives a task's `workflow_state` through a loaded
//! `WorkflowDefinition`. The graph's topology comes entirely from the
//! definition (§5.1); this module only supplies the fixed, small
//! vocabulary of stage *behaviors* (§5.2): `agent_turn`, `human_gate`,
//! `terminal` (P1-7) and `shell` (P2-1). `poll` is already parsed by the
//! loader (P1-6) but its execution lands with P2-2, so entering one here
//! is a deliberate, reported error rather than a silent no-op.
//!
//! Two kinds — `agent_turn` and `shell` — do work that outlives the call
//! that started them, and both hand their outcome back the same way: they
//! return from `enter_stage` as soon as the work is under way and a
//! detached task calls `advance` once it finishes. That indirection is
//! load-bearing, not stylistic. `enter_stage` runs inside the per-task
//! lock that `advance` re-acquires, and `tokio::sync::Mutex` is not
//! reentrant, so a stage kind that blocked here and advanced inline would
//! wedge its task forever.
//!
//! `loop_guard` bookkeeping (§5.3) lives entirely in `workflow_state.
//! loop_counters`, keyed by stage name to `{ entered_from, count }`:
//! `count` is how many times that stage has been left via its guarded
//! outcome since it was last entered from a *different* prior stage (the
//! reset condition in §5.3) — see `bump_loop_counter`/`note_stage_entry`.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chokofactory_core::models::{EventType, Task, TaskRunEndReason, TaskRunStatus};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify};

use crate::db::{events, projects, task_runs, tasks, workflow_state};
use crate::global_config::{GlobalConfig, GlobalConfigError};
use crate::poll;
use crate::role_config::{self, RoleConfigError};
use crate::session::{SessionError, SessionManager};
use crate::shell;
use crate::workflow_def::{
    Capture, PollOutcome, ShellCommand, StageDef, StageKind, WorkflowDefError, WorkflowDefinition,
};

/// How often the `agent_turn` completion watcher polls a `task_run`'s
/// status. Not configurable (yet) — this is an internal implementation
/// detail of auto-advancing single-shot turns, not a user-facing knob.
const TURN_WATCH_INTERVAL: Duration = Duration::from_millis(100);

pub struct WorkflowEngine {
    pool: SqlitePool,
    session_manager: Arc<SessionManager>,
    /// Serializes `advance()` calls per task (§ review on PR #35): without
    /// this, two callers racing to advance the same task's `workflow_state`
    /// (e.g. the turn-completion watcher and `send_message_or_resume`'s
    /// `human_gate` relay) could both read the same row and then both
    /// write, silently clobbering one call's `loop_counters`.
    task_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Where `create_task`/`send_message` resolve a `workflow_def` name to
    /// a file (P1-8 LLD §2.6/§2.8) — the *seeded* user directory
    /// (`~/.config/chokofactory/workflows/` in production), never a
    /// repo-relative path.
    workflows_dir: PathBuf,
    /// `None` means "no global config file configured" (e.g. `$HOME`
    /// unset, or a test that doesn't care) — treated the same as a
    /// missing file: role resolution just gets no global defaults.
    global_config_path: Option<PathBuf>,
    /// Woken after a stage transition is recorded (X-3), so the live-events
    /// WebSocket pushes it immediately. The same `Notify` the
    /// `SessionManager` signals for session events — a stage transition is
    /// just another entry in the one timeline both write to, and without
    /// this a `human_gate`-only workflow would sit silent until some
    /// unrelated event happened to arrive.
    events_notify: Arc<Notify>,
}

#[derive(Debug)]
pub enum EngineError {
    NoWorkflowState,
    NoSuchTask,
    UnknownStage(String),
    UnknownOutcome {
        stage: String,
        outcome: String,
    },
    /// A detached runner finished work for a stage the task has since left,
    /// so its outcome was discarded rather than applied to whatever stage
    /// is current now.
    StageMovedOn {
        expected: String,
        actual: String,
    },
    TerminalStageHasNoTransitions(String),
    MissingAgentTurnInput(String),
    UnknownRole {
        stage: String,
        role: String,
    },
    /// A `poll` stage whose `outcomes:` pattern doesn't compile.
    /// `WorkflowDefinition::validate` rejects these at load, so this is
    /// only reachable for a definition built by hand — see `poll::compile`.
    InvalidPollPattern {
        stage: String,
        reason: String,
    },
    Session(SessionError),
    Db(sqlx::Error),
    Io(std::io::Error),
    GlobalConfig(GlobalConfigError),
    RoleConfig(RoleConfigError),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NoWorkflowState => write!(f, "task has no workflow_state row"),
            EngineError::NoSuchTask => write!(f, "no such task"),
            EngineError::UnknownStage(stage) => {
                write!(f, "workflow_state references unknown stage '{stage}'")
            }
            EngineError::UnknownOutcome { stage, outcome } => write!(
                f,
                "stage '{stage}' has no 'on:' transition for outcome '{outcome}'"
            ),
            EngineError::StageMovedOn { expected, actual } => write!(
                f,
                "task left stage '{expected}' (now in '{actual}') before its outcome could be applied"
            ),
            EngineError::TerminalStageHasNoTransitions(stage) => {
                write!(f, "stage '{stage}' is terminal and cannot be advanced")
            }
            EngineError::MissingAgentTurnInput(stage) => write!(
                f,
                "stage '{stage}' is an agent_turn with no prompt_file and no input was supplied"
            ),
            EngineError::UnknownRole { stage, role } => write!(
                f,
                "stage '{stage}' is an agent_turn with unknown role '{role}'"
            ),
            EngineError::InvalidPollPattern { stage, reason } => write!(
                f,
                "stage '{stage}' has a poll outcome pattern that does not compile: {reason}"
            ),
            EngineError::Session(err) => write!(f, "{err}"),
            EngineError::Db(err) => write!(f, "{err}"),
            EngineError::Io(err) => write!(f, "{err}"),
            EngineError::GlobalConfig(err) => write!(f, "{err}"),
            EngineError::RoleConfig(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<sqlx::Error> for EngineError {
    fn from(err: sqlx::Error) -> Self {
        EngineError::Db(err)
    }
}

/// A `workflow_def` name resolved to a file under a `WorkflowEngine`'s
/// `workflows_dir` (P1-8 LLD §2.8). Deliberately an allowlist
/// (`^[A-Za-z0-9_-]+$`), not the workflow loader's absolute-path/`..`
/// blocklist (`workflow_def.rs::resolve_file`/`fileref::resolve_relative`):
/// `name` is a single opaque identifier that will eventually arrive
/// straight from an HTTP request body (#9) or CLI arg (#10), materially
/// less trusted than a relative path written into a workflow file already
/// sitting on disk — an allowlist leaves no path syntax to reason about.
fn resolve_workflow_path(workflows_dir: &Path, name: &str) -> Result<PathBuf, ResolveError> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_name {
        return Err(ResolveError::InvalidName(name.to_string()));
    }
    let path = workflows_dir.join(format!("{name}.yaml"));
    if !path.is_file() {
        return Err(ResolveError::NotFound(name.to_string()));
    }
    Ok(path)
}

#[derive(Debug)]
pub enum ResolveError {
    InvalidName(String),
    NotFound(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::InvalidName(name) => {
                write!(
                    f,
                    "'{name}' is not a valid workflow name (expected only letters, digits, '_', '-')"
                )
            }
            ResolveError::NotFound(name) => write!(f, "no workflow named '{name}' was found"),
        }
    }
}

impl std::error::Error for ResolveError {}

#[derive(Debug)]
pub enum CreateTaskError {
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// `project_id` doesn't reference an existing project. Checked
    /// explicitly (P1-9 review) rather than left to surface as whatever
    /// `sqlx::Error` a raw `tasks.project_id` foreign-key violation
    /// produces — the same care `db::projects::delete`'s own caller
    /// already takes for the opposite direction of that same FK.
    NoSuchProject(String),
    /// `parent_task_id` was supplied but doesn't reference an existing
    /// task — same reasoning as `NoSuchProject`.
    NoSuchParentTask(String),
    Db(sqlx::Error),
    Start(EngineError),
}

impl fmt::Display for CreateTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateTaskError::Resolve(err) => write!(f, "{err}"),
            CreateTaskError::WorkflowDef(err) => write!(f, "{err}"),
            CreateTaskError::NoSuchProject(id) => write!(f, "no such project '{id}'"),
            CreateTaskError::NoSuchParentTask(id) => write!(f, "no such parent task '{id}'"),
            CreateTaskError::Db(err) => write!(f, "{err}"),
            CreateTaskError::Start(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CreateTaskError {}

impl From<sqlx::Error> for CreateTaskError {
    fn from(err: sqlx::Error) -> Self {
        CreateTaskError::Db(err)
    }
}

#[derive(Debug)]
pub enum SendMessageError {
    NoSuchTask,
    NoWorkflowState,
    UnknownStage(String),
    UnknownRole {
        stage: String,
        role: String,
    },
    /// The task's current stage isn't a standing-open `agent_turn` (empty
    /// `on:`) — it's either a different kind, or an `agent_turn` that
    /// *can* transition. Callers that don't already know the stage kind
    /// should go through `send_message_or_resume` instead, which picks
    /// between this and `advance`'s `human_gate` relay — see P1-8 LLD
    /// §4.3 for why this is a hard boundary, not a Phase-1 gap.
    StageNotOpenEnded(String),
    /// The stage is open-ended, but no `task_run` has ever been recorded
    /// for it (e.g. `create_task`'s `start_task` failed before spawning
    /// one).
    NoOpenRun(String),
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    RoleConfig(RoleConfigError),
    GlobalConfig(GlobalConfigError),
    Session(SessionError),
    Db(sqlx::Error),
}

impl fmt::Display for SendMessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendMessageError::NoSuchTask => write!(f, "no such task"),
            SendMessageError::NoWorkflowState => write!(f, "task has no workflow_state row"),
            SendMessageError::UnknownStage(stage) => {
                write!(f, "workflow_state references unknown stage '{stage}'")
            }
            SendMessageError::UnknownRole { stage, role } => write!(
                f,
                "stage '{stage}' is an agent_turn with unknown role '{role}'"
            ),
            SendMessageError::StageNotOpenEnded(stage) => write!(
                f,
                "stage '{stage}' can transition to another stage, so it cannot accept a relayed message here"
            ),
            SendMessageError::NoOpenRun(stage) => {
                write!(f, "stage '{stage}' has no task_run recorded for it yet")
            }
            SendMessageError::Resolve(err) => write!(f, "{err}"),
            SendMessageError::WorkflowDef(err) => write!(f, "{err}"),
            SendMessageError::RoleConfig(err) => write!(f, "{err}"),
            SendMessageError::GlobalConfig(err) => write!(f, "{err}"),
            SendMessageError::Session(err) => write!(f, "{err}"),
            SendMessageError::Db(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SendMessageError {}

impl From<sqlx::Error> for SendMessageError {
    fn from(err: sqlx::Error) -> Self {
        SendMessageError::Db(err)
    }
}

/// Errors from [`WorkflowEngine::send_message_or_resume`] — the dispatch
/// this stage's own doc comments (and `SendMessageError::StageNotOpenEnded`'s)
/// call out as "issue #9's job": relay a human message into whichever of
/// `send_message`/`advance` the task's current stage actually needs.
#[derive(Debug)]
pub enum SendMessageOrResumeError {
    NoSuchTask,
    NoWorkflowState,
    UnknownStage(String),
    /// The current stage is neither a standing-open `agent_turn` nor a
    /// `human_gate` — e.g. `shell`/`poll`/`terminal`, or an `agent_turn`
    /// that can itself transition (not yet a case this dispatch handles).
    UnsupportedStageKind(String),
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    Db(sqlx::Error),
    SendMessage(SendMessageError),
    Advance(EngineError),
}

impl fmt::Display for SendMessageOrResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendMessageOrResumeError::NoSuchTask => write!(f, "no such task"),
            SendMessageOrResumeError::NoWorkflowState => {
                write!(f, "task has no workflow_state row")
            }
            SendMessageOrResumeError::UnknownStage(stage) => {
                write!(f, "workflow_state references unknown stage '{stage}'")
            }
            SendMessageOrResumeError::UnsupportedStageKind(stage) => write!(
                f,
                "stage '{stage}' cannot accept a message or resume signal here"
            ),
            SendMessageOrResumeError::Resolve(err) => write!(f, "{err}"),
            SendMessageOrResumeError::WorkflowDef(err) => write!(f, "{err}"),
            SendMessageOrResumeError::Db(err) => write!(f, "{err}"),
            SendMessageOrResumeError::SendMessage(err) => write!(f, "{err}"),
            SendMessageOrResumeError::Advance(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SendMessageOrResumeError {}

impl From<sqlx::Error> for SendMessageOrResumeError {
    fn from(err: sqlx::Error) -> Self {
        SendMessageOrResumeError::Db(err)
    }
}

impl WorkflowEngine {
    pub fn new(
        pool: SqlitePool,
        session_manager: Arc<SessionManager>,
        workflows_dir: PathBuf,
        global_config_path: Option<PathBuf>,
        events_notify: Arc<Notify>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            session_manager,
            task_locks: Mutex::new(HashMap::new()),
            workflows_dir,
            global_config_path,
            events_notify,
        })
    }

    /// Missing `global_config_path` (not configured) and a missing file at
    /// a configured path are both just "no global defaults" — not cached
    /// (P1-8 LLD §4.5): re-read and re-parsed on every call.
    fn load_global_config(&self) -> Result<GlobalConfig, GlobalConfigError> {
        match &self.global_config_path {
            Some(path) => GlobalConfig::load(path),
            None => Ok(GlobalConfig::default()),
        }
    }

    /// Creates a task under `project_id` running `workflow_def_name`,
    /// feeding `initial_input` in as the entry stage's first message
    /// (P1-8 LLD §2.7). `config` is the task-level override layer
    /// `role_config::resolve` reads (`config.roles.<name>.*`, plus the
    /// task-wide `config.cwd`).
    ///
    /// `workflow_def_name` is resolved and the definition freshly loaded
    /// on every call, not cached (P1-8 LLD §4.5) — the same file `WorkflowEngine`
    /// would otherwise have to invalidate a cache entry for.
    ///
    /// `parent_task_id` tags this task as spawned via delegation (§6.2's
    /// `choco task create --parent-task <id>`) — purely a label for the UI
    /// and for a parent task to poll; it has no effect on how this task's
    /// own workflow runs.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_task(
        self: &Arc<Self>,
        project_id: &str,
        parent_task_id: Option<&str>,
        workflow_def_name: &str,
        title: &str,
        initial_input: &str,
        config: Value,
    ) -> Result<Task, CreateTaskError> {
        let path = resolve_workflow_path(&self.workflows_dir, workflow_def_name)
            .map_err(CreateTaskError::Resolve)?;
        let definition =
            Arc::new(WorkflowDefinition::load(&path).map_err(CreateTaskError::WorkflowDef)?);

        // Checked explicitly rather than left to surface as a raw FK
        // violation from the `INSERT` below (P1-9 review): both columns
        // are foreign keys (`tasks.project_id`/`tasks.parent_task_id`),
        // and `db::pool::connect` enables `foreign_keys`, so a bad id
        // would otherwise fail as an opaque `sqlx::Error` instead of a
        // reported, specific error the API layer can map to 404.
        if projects::get(&self.pool, project_id).await?.is_none() {
            return Err(CreateTaskError::NoSuchProject(project_id.to_string()));
        }
        if let Some(parent_id) = parent_task_id
            && tasks::get(&self.pool, parent_id).await?.is_none()
        {
            return Err(CreateTaskError::NoSuchParentTask(parent_id.to_string()));
        }

        let task = tasks::create(
            &self.pool,
            tasks::NewTask {
                project_id,
                parent_task_id,
                workflow_def: workflow_def_name,
                title,
                config,
            },
        )
        .await?;

        self.start_task(&task.id, &definition, Some(initial_input))
            .await
            .map_err(CreateTaskError::Start)?;

        Ok(task)
    }

    /// Feeds a follow-up human message into `task_id`'s current stage,
    /// which must be a standing-open `agent_turn` (empty `on:` — never
    /// advances, so there's no risk of the stage changing out from under
    /// this lookup, P1-8 LLD §4.3). Anything else — a different kind, or
    /// an `agent_turn` that *can* transition — is rejected rather than
    /// silently racing a concurrent `advance()`; callers that don't already
    /// know the stage kind should go through `send_message_or_resume`.
    pub async fn send_message(
        self: &Arc<Self>,
        task_id: &str,
        text: &str,
    ) -> Result<(), SendMessageError> {
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageError::NoSuchTask)?;

        let path = resolve_workflow_path(&self.workflows_dir, &task.workflow_def)
            .map_err(SendMessageError::Resolve)?;
        let definition = WorkflowDefinition::load(&path).map_err(SendMessageError::WorkflowDef)?;

        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageError::NoWorkflowState)?;
        let current_stage = state.current_stage;

        let stage_def = definition
            .stages
            .get(&current_stage)
            .ok_or_else(|| SendMessageError::UnknownStage(current_stage.clone()))?;

        let StageKind::AgentTurn { role, .. } = &stage_def.kind else {
            return Err(SendMessageError::StageNotOpenEnded(current_stage));
        };
        if !stage_def.on.is_empty() {
            return Err(SendMessageError::StageNotOpenEnded(current_stage));
        }

        // Same defensive check as `enter_agent_turn`'s: the loader rejects
        // an agent_turn stage with an unknown role, but `roles`/`stages`
        // are `pub` fields with no private-construction guard, so a
        // definition built by hand could still reach here unvalidated
        // (§ review on PR #35).
        let role_def = definition
            .roles
            .get(role)
            .ok_or_else(|| SendMessageError::UnknownRole {
                stage: current_stage.clone(),
                role: role.clone(),
            })?;

        let task_run = task_runs::get_current_for_stage(&self.pool, task_id, &current_stage)
            .await?
            .ok_or_else(|| SendMessageError::NoOpenRun(current_stage.clone()))?;

        let global = self
            .load_global_config()
            .map_err(SendMessageError::GlobalConfig)?;
        let resolved = role_config::resolve(role, role_def, &global, &task.config, task_cwd(&task))
            .map_err(SendMessageError::RoleConfig)?;

        // Recorded *before* handing off to the live session, not after —
        // once handed off, the session's own drain task can react and
        // append its reply's events at any point, on any thread. Recording
        // first guarantees this event's `(created_at, id)` always sorts
        // before anything that reply could produce, regardless of
        // scheduling; recording after would leave the two racing, with no
        // ordering guarantee under a real multi-threaded runtime (a
        // sequential-looking "send_message" test can hide this, since a
        // single-threaded test runtime happens not to schedule the drain
        // task until this task yields). Best-effort like `drain_session`'s
        // own event-append calls: a transient DB failure here shouldn't
        // block the relay that follows.
        if let Err(err) = events::append(
            &self.pool,
            &task_run.id,
            EventType::HumanMessage,
            json!({ "text": text }),
        )
        .await
        {
            tracing::error!(task_run_id = %task_run.id, %err, "failed to record human message event");
        }

        self.session_manager
            .send_message(&task_run.id, text, &resolved.role_config)
            .await
            .map_err(SendMessageError::Session)
    }

    /// Dispatches a human message against `task_id`'s current stage to
    /// whichever of `send_message`/`advance` it actually needs (P1-9): a
    /// standing-open `agent_turn` relays `text` straight into its live
    /// session via `send_message`; a `human_gate` has no session to relay
    /// into at all — the human's `text` is the resume signal itself, so
    /// this calls `advance(task_id, definition, "resumed")` instead. Any
    /// other stage kind (a mid-transition `agent_turn`, `shell`, `poll`,
    /// `terminal`) is rejected rather than guessing.
    ///
    /// This re-loads `task`/`workflow_state`/the workflow definition itself
    /// before delegating to a primitive that re-loads them again —
    /// redundant, but consistent with `send_message`'s own "not cached,
    /// freshly loaded on every call" stance (P1-8 LLD §4.5), and cheap for
    /// a single-user local daemon.
    pub async fn send_message_or_resume(
        self: &Arc<Self>,
        task_id: &str,
        text: &str,
    ) -> Result<(), SendMessageOrResumeError> {
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageOrResumeError::NoSuchTask)?;

        let path = resolve_workflow_path(&self.workflows_dir, &task.workflow_def)
            .map_err(SendMessageOrResumeError::Resolve)?;
        let definition = Arc::new(
            WorkflowDefinition::load(&path).map_err(SendMessageOrResumeError::WorkflowDef)?,
        );

        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageOrResumeError::NoWorkflowState)?;
        let current_stage = state.current_stage;

        let stage_def = definition
            .stages
            .get(&current_stage)
            .ok_or_else(|| SendMessageOrResumeError::UnknownStage(current_stage.clone()))?;

        match &stage_def.kind {
            StageKind::HumanGate => self
                .advance(task_id, &definition, "resumed")
                .await
                .map_err(SendMessageOrResumeError::Advance),
            StageKind::AgentTurn { .. } if stage_def.on.is_empty() => self
                .send_message(task_id, text)
                .await
                .map_err(SendMessageOrResumeError::SendMessage),
            _ => Err(SendMessageOrResumeError::UnsupportedStageKind(
                current_stage,
            )),
        }
    }

    /// Returns (creating if needed) the lock guarding `task_id`'s
    /// `workflow_state` read-modify-write in `advance()`.
    async fn lock_for_task(&self, task_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.task_locks.lock().await;
        Arc::clone(
            locks
                .entry(task_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Removes `task_id`'s entry from `task_locks`, but only if `lock` is
    /// the sole outstanding reference to it.
    ///
    /// Unconditional removal is unsound with 3+ overlapping callers: if
    /// another caller (B) already cloned this same `Arc` from the map
    /// before this call started evicting, removing the map entry now
    /// doesn't affect B — B still holds/awaits the *same* `Arc` — but a
    /// brand-new caller (C) arriving after the removal gets handed a
    /// freshly-inserted, unrelated `Arc`, and now B and C can run their
    /// `workflow_state` read-modify-writes concurrently on two different
    /// mutexes, exactly the lost-update race `task_locks` exists to
    /// prevent (§ review on PR #35). Checking `strong_count` while still
    /// holding `task_locks`'s own guard (so no one can clone the `Arc` out
    /// from under this check) tells us whether such a B exists: the
    /// baseline is 2 — this call's local `lock` binding, plus the map's
    /// own stored clone — so anything higher means another caller is
    /// still referencing it and eviction must be skipped, leaving that
    /// caller (and whoever joins after it) to eventually evict instead.
    async fn evict_task_lock_if_unshared(&self, task_id: &str, lock: &Arc<Mutex<()>>) {
        let mut locks = self.task_locks.lock().await;
        if Arc::strong_count(lock) <= 2 {
            locks.remove(task_id);
        }
    }

    /// Creates `task_id`'s `workflow_state` row at `definition`'s entry
    /// stage (§5.1: the first stage declared) and enters it.
    /// `initial_input` is the human-typed message a chat-style task was
    /// created with (§5.4) — used only if the entry stage is an
    /// `agent_turn` with no `prompt_file`; ignored otherwise.
    pub async fn start_task(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        initial_input: Option<&str>,
    ) -> Result<(), EngineError> {
        // Takes the same per-task lock `advance()` uses (§ review on PR
        // #35): nothing can call `advance()` before this creates
        // `workflow_state` below, but holding it anyway removes the need
        // to reason about that ordering as a standing invariant — e.g. a
        // retry that calls `start_task` again while an earlier attempt is
        // still mid-`enter_stage` can't race a concurrent `advance()`.
        let lock = self.lock_for_task(task_id).await;
        let _guard = lock.lock().await;

        let start = definition.start_stage();
        let result: Result<(), EngineError> = async {
            workflow_state::create(&self.pool, task_id, start).await?;
            self.enter_stage(task_id, definition, start, initial_input, None)
                .await
        }
        .await;
        // Evict on any error (either nothing was written yet, or
        // `workflow_state` already durably committed before `enter_stage`
        // ran — a fresh lock next time reads that same state correctly
        // either way) or once the entry stage is itself terminal (no
        // future call for this task will ever come). `lock_for_task`
        // guards against a still-referenced `Arc` actually being removed
        // (§ review on PR #35).
        let entry_stage_is_terminal = definition
            .stages
            .get(start)
            .is_some_and(|stage_def| matches!(stage_def.kind, StageKind::Terminal));
        if result.is_err() || entry_stage_is_terminal {
            self.evict_task_lock_if_unshared(task_id, &lock).await;
        }
        result
    }

    /// Applies `outcome` against the task's current stage — looking it up
    /// in that stage's `on:` map and running any `loop_guard` (§5.3) —
    /// transitions `workflow_state`, and enters whatever stage results.
    ///
    /// Callers: the `agent_turn` completion watcher spawned by
    /// `enter_stage` (for a plain single-shot turn's `done`), and
    /// `send_message_or_resume`'s `human_gate` relay (its `resumed`).
    pub async fn advance(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        outcome: &str,
    ) -> Result<(), EngineError> {
        self.advance_from_stage(task_id, definition, outcome, None, None)
            .await
    }

    /// [`Self::advance`] for a caller that ran detached work for a specific
    /// stage: it only applies `outcome` if the task is still *in*
    /// `expected_stage`, and stores `capture` (a `shell` stage's `capture:`,
    /// §5.1) into `workflow_state.payload` under the stage it transitions
    /// *from* — which, once the check below has passed, is
    /// `expected_stage`. (Passing a `capture` with no `expected_stage` would
    /// key it to whatever stage happened to be current; no caller does, and
    /// the check exists so none can do it unknowingly.)
    ///
    /// The capture is threaded *through* the transition rather than written
    /// by the caller beforehand, and that is the whole point of this
    /// function existing. `workflow_state::update` rewrites the entire row,
    /// so a caller that read the state, merged its capture, and wrote it
    /// back would be doing a read-modify-write outside this function's
    /// per-task lock — and a transition interleaving between that read and
    /// write would silently lose either the capture or the new
    /// `current_stage`/`loop_counters`. Merging here keeps all three fields
    /// in the single UPDATE already made under the lock.
    ///
    /// `expected_stage` makes the detached runner's assumption explicit
    /// rather than merely true-by-construction. No path today can move a
    /// task out of a stage while its runner is still in flight, but the
    /// window is only as short as the command; P2-2's `poll` will hold it
    /// open for an `interval`/`timeout` at a time, and an outcome applied
    /// to whatever stage happened to be current by then would be a
    /// transition nobody asked for.
    async fn advance_from_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        outcome: &str,
        expected_stage: Option<&str>,
        capture: Option<Value>,
    ) -> Result<(), EngineError> {
        let lock = self.lock_for_task(task_id).await;
        let _guard = lock.lock().await;

        // The stage entered on success, so the caller below can tell
        // whether it just became terminal without a second query.
        let result: Result<String, EngineError> =
            async {
                let state = workflow_state::get(&self.pool, task_id)
                    .await?
                    .ok_or(EngineError::NoWorkflowState)?;
                let from_stage = state.current_stage.clone();

                // Checked inside the lock, against the same read the
                // transition below is computed from — outside it, the
                // answer could go stale before it was used.
                if let Some(expected) = expected_stage
                    && from_stage != expected
                {
                    return Err(EngineError::StageMovedOn {
                        expected: expected.to_string(),
                        actual: from_stage,
                    });
                }

                let stage_def = definition
                    .stages
                    .get(&from_stage)
                    .ok_or_else(|| EngineError::UnknownStage(from_stage.clone()))?;

                if matches!(stage_def.kind, StageKind::Terminal) {
                    return Err(EngineError::TerminalStageHasNoTransitions(from_stage));
                }

                let mut next_stage = stage_def.on.get(outcome).cloned().ok_or_else(|| {
                    EngineError::UnknownOutcome {
                        stage: from_stage.clone(),
                        outcome: outcome.to_string(),
                    }
                })?;

                let mut loop_counters = state.loop_counters;
                if let Some(guard) = &stage_def.loop_guard
                    && guard.on == outcome
                {
                    let count = bump_loop_counter(&mut loop_counters, &from_stage);
                    if count > u64::from(guard.max) {
                        next_stage = guard.then.clone();
                        reset_loop_count(&mut loop_counters, &from_stage);
                    }
                }
                note_stage_entry(&mut loop_counters, definition, &next_stage, &from_stage);

                let mut payload = state.payload;
                if let Some(value) = capture {
                    // Keyed by the stage that produced it, which the check
                    // above has confirmed is still the current one.
                    merge_stage_capture(&mut payload, &from_stage, value);
                }

                workflow_state::update(
                    &self.pool,
                    task_id,
                    workflow_state::WorkflowStateUpdate {
                        current_stage: next_stage.clone(),
                        loop_counters,
                        payload,
                    },
                )
                .await?;

                // `enter_stage` records the transition itself (X-3), so the
                // trail this used to push onto `workflow_state.stage_history`
                // now lives in the events timeline with a timestamp and the
                // outcome that caused it.
                self.enter_stage(task_id, definition, &next_stage, None, Some(outcome))
                    .await?;
                Ok(next_stage)
            }
            .await;
        // Same rationale as `start_task`'s eviction above: every error
        // branch here either precedes any write (nothing to protect) or
        // follows `workflow_state::update` already having durably
        // committed (a fresh lock next time reads that same state
        // correctly), so it's safe to evict on failure; likewise once the
        // stage just entered is terminal, no future call for this task
        // will ever come. `evict_task_lock_if_unshared` guards against
        // removing an `Arc` some other overlapping caller still holds
        // (§ review on PR #35).
        let entered_terminal_stage = result.as_ref().is_ok_and(|stage| {
            definition
                .stages
                .get(stage)
                .is_some_and(|stage_def| matches!(stage_def.kind, StageKind::Terminal))
        });
        if result.is_err() || entered_terminal_stage {
            self.evict_task_lock_if_unshared(task_id, &lock).await;
        }
        result.map(|_| ())
    }

    /// Dispatches the behavior for whichever kind `stage_name` is (§5.2),
    /// and records the transition into it on the task's timeline (X-3).
    /// `input` is only consulted for a `prompt_file`-less `agent_turn`.
    /// `entered_via` is the outcome that selected this stage — `None` when
    /// it's the task's entry stage, which nothing transitioned into.
    async fn enter_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        input: Option<&str>,
        entered_via: Option<&str>,
    ) -> Result<(), EngineError> {
        let stage_def = definition
            .stages
            .get(stage_name)
            .ok_or_else(|| EngineError::UnknownStage(stage_name.to_string()))?;

        // Every stage kind funnels through here, so recording the
        // transition once at this point covers `start_task`'s entry stage,
        // every `advance`, and terminal entry alike (X-3). Placed after
        // `stage_def` resolves so an unknown stage doesn't record a
        // transition that never happened, but before dispatching on the
        // kind so it's unconditional — `workflow_state.current_stage` is
        // already committed by the caller regardless of whether this engine
        // can execute that kind yet.
        //
        // Best-effort, not `?`: the same reasoning as the `human_message`
        // append below and the terminal-stage `update_status` further down
        // (§ review on PR #35). The state transition is already durable, so
        // returning early can't undo it — it would only skip the caller's
        // lock-eviction check and abort a stage that has, in fact, been
        // entered.
        //
        // Note this is two writes, not one: the caller commits
        // `workflow_state` and then this records the trail entry, where the
        // old `stage_history` column was updated in the same statement as
        // `current_stage`. A crash or SQLITE_BUSY in between drops a
        // transition from the timeline permanently, and with no per-session
        // counter left there's no gap to detect it by. Making the pair
        // atomic means threading a transaction from `advance`/`start_task`
        // through this function; deliberately not done here (X-3), since
        // `current_stage` — the value the engine actually reads back — is
        // the one that must be durable, and the failure is logged loudly.
        match events::append_stage_transition(&self.pool, task_id, stage_name, entered_via).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, %err,
                "failed to record stage transition event"
            ),
        }

        match &stage_def.kind {
            StageKind::AgentTurn { role, prompt_file } => {
                self.enter_agent_turn(
                    task_id,
                    definition,
                    stage_name,
                    stage_def,
                    role,
                    prompt_file.as_deref(),
                    input,
                )
                .await
            }
            // Pauses the task with nothing further to do here; whatever
            // relays the next human message is responsible for calling
            // `advance(task_id, definition, "resumed")` once it arrives.
            StageKind::HumanGate => Ok(()),
            StageKind::Terminal => {
                // Best-effort, not `?`: propagating this would skip the
                // caller's (`advance`/`start_task`) lock-eviction check for
                // a terminal stage, and `workflow_state` is already
                // committed to this stage regardless, so a transient
                // failure here can't be un-done by returning early anyway
                // (§ review on PR #35). Lock eviction itself happens in
                // the caller, which knows the stage just entered and can
                // safely check whether any other overlapping caller still
                // references it (`evict_task_lock_if_unshared`) — this
                // function has no access to that `Arc`.
                if let Err(err) = tasks::update_status(&self.pool, task_id, "closed").await {
                    tracing::error!(
                        task_id, %err,
                        "failed to mark task closed after entering a terminal stage"
                    );
                } else {
                    tracing::info!(
                        task_id,
                        stage = stage_name,
                        "task closed (entered terminal stage)"
                    );
                }
                Ok(())
            }
            StageKind::Shell {
                command,
                capture,
                timeout,
            } => {
                self.enter_shell(
                    task_id,
                    definition,
                    stage_name,
                    command.clone(),
                    *capture,
                    *timeout,
                )
                .await
            }
            StageKind::Poll {
                command,
                capture,
                interval,
                timeout,
                outcomes,
            } => {
                self.enter_poll(
                    task_id,
                    definition,
                    stage_name,
                    command.clone(),
                    *capture,
                    *interval,
                    *timeout,
                    outcomes,
                )
                .await
            }
        }
    }

    /// Starts a `shell` stage's command (§5.2) and returns immediately; the
    /// outcome arrives later, from the detached runner below.
    ///
    /// Running the command inline here instead would deadlock the task
    /// permanently: `enter_stage` is called from inside the per-task lock
    /// held by `advance`/`start_task`, the runner has to call `advance` to
    /// report its outcome, and `tokio::sync::Mutex` is not reentrant. The
    /// `agent_turn` path has the same constraint and resolves it the same
    /// way, via `spawn_turn_watcher`.
    async fn enter_shell(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        command: ShellCommand,
        capture: Option<Capture>,
        timeout: Option<Duration>,
    ) -> Result<(), EngineError> {
        // Resolved here rather than in the spawned task so that a missing
        // task fails the transition that caused it, where the caller can
        // still see the error, instead of only reaching a log line.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let cwd = task_cwd(&task);

        self.spawn_shell_runner(
            task_id.to_string(),
            Arc::clone(definition),
            stage_name.to_string(),
            command,
            capture,
            timeout,
            cwd,
        );
        Ok(())
    }

    /// Deliberately a *synchronous* fn, like `spawn_turn_watcher`. The
    /// spawned future eventually calls `advance` → `enter_stage` →
    /// `enter_shell`, i.e. back to here; discharging `tokio::spawn`'s
    /// `Send` obligation from inside an `async fn` would make that cycle
    /// part of the compiler's auto-trait inference for `enter_shell`'s own
    /// future and fail to resolve. A sync fn's body is checked
    /// independently, which breaks the cycle.
    #[allow(clippy::too_many_arguments)]
    fn spawn_shell_runner(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        command: ShellCommand,
        capture: Option<Capture>,
        timeout: Option<Duration>,
        cwd: PathBuf,
    ) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine
                .run_shell_stage(
                    &task_id,
                    &definition,
                    &stage_name,
                    command,
                    capture,
                    timeout,
                    cwd,
                )
                .await;
        });
    }

    /// Runs the command, records what it did on the task's timeline, and
    /// advances the task with `done`/`error` (§5.2) carrying any capture.
    #[allow(clippy::too_many_arguments)]
    async fn run_shell_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        command: ShellCommand,
        capture: Option<Capture>,
        timeout: Option<Duration>,
        cwd: PathBuf,
    ) {
        let described = describe_command(&command);

        // A failure to run the command at all is reported as the stage's
        // `error` outcome rather than dropped: from the workflow's point of
        // view "the command could not be run" and "the command ran and
        // failed" both mean this stage did not succeed, and a task whose
        // `on: error` edge exists should follow it either way. The reason
        // goes on the timeline, since it's the only place an operator would
        // find it.
        let started = std::time::Instant::now();
        let outcome = match shell::run(&command, &cwd, timeout).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // The two variants mean materially different things to
                // whoever reads this: `Spawn` means nothing ran, while `Io`
                // means the command *did* run — possibly for a long time,
                // possibly mutating the working copy — and only reading its
                // output failed. Reporting both as "could not be started"
                // would actively mislead. Two call sites rather than one
                // interpolated message, so each stays a static string that
                // log aggregation can group on.
                match err {
                    shell::ShellError::Spawn(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command could not be started"
                    ),
                    shell::ShellError::Io(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command ran but its output could not be read"
                    ),
                }
                self.append_command_event(
                    task_id,
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "exit_code": Value::Null,
                        "timed_out": false,
                        "duration_ms": elapsed_ms(started),
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "note": err.to_string(),
                    }),
                )
                .await;
                self.finish_shell_stage(task_id, definition, stage_name, "error", None)
                    .await;
                return;
            }
        };

        let stage_outcome = if outcome.succeeded() { "done" } else { "error" };

        // Only a command that succeeded contributes a capture. A failed one
        // has nothing worth handing to a later stage, and storing it anyway
        // would be actively harmful: `stages.<name>` is keyed by stage, so
        // a stage re-entered by a retry loop would overwrite the good value
        // from the attempt that worked with the failed attempt's output (or,
        // on a timeout, with an empty string). A later
        // `{{ stages.open_pr.number }}` would then resolve against garbage.
        // The command's output is still on the timeline either way.
        let (captured, mut note) = if outcome.succeeded() {
            derive_capture(capture, &outcome.stdout, task_id, stage_name)
        } else if capture.is_some() {
            (
                None,
                Some("stdout not captured: the command did not succeed".to_string()),
            )
        } else {
            (None, None)
        };

        if outcome.timed_out {
            tracing::warn!(
                task_id,
                stage = stage_name,
                escaped = outcome.escaped,
                "shell stage command exceeded its timeout and was killed"
            );
        }
        // Outranks any capture note: the workflow is about to follow its
        // `on: error` edge, quite possibly straight back into this same
        // command, while the last one is still running in the same working
        // copy. The timeline is where an operator would find that out.
        //
        // Worded for what both `escaped` arms actually know — one saw the
        // group outlive SIGKILL, the other only failed to read its pipes —
        // and kept short enough to survive `choco task events`' 100-char
        // line budget, since being truncated before "may still be running"
        // would defeat the point of recording it.
        if outcome.escaped {
            note = Some(
                "could not confirm the process group exited — something may still be running"
                    .to_string(),
            );
        }

        let mut payload = json!({
            "stage": stage_name,
            "command": described,
            "exit_code": outcome.exit_code,
            "timed_out": outcome.timed_out,
            // A queryable sibling of `timed_out` rather than only a phrase
            // inside `note`, so "did any stage leave something running?" is
            // answerable from `choco --json` without matching free text.
            "escaped": outcome.escaped,
            "duration_ms": duration_ms(outcome.duration),
            "stdout_tail": tail(&outcome.stdout),
            "stderr_tail": tail(&outcome.stderr),
        });
        if let Some(note) = note {
            payload["note"] = Value::String(note);
        }
        self.append_command_event(task_id, payload).await;

        self.finish_shell_stage(task_id, definition, stage_name, stage_outcome, captured)
            .await;
    }

    /// Records what a `shell` or `poll` stage's command did.
    ///
    /// Best-effort, like every other event append in this module: the
    /// command has already run, so failing to record it can't be undone by
    /// refusing to transition — and refusing would strand the task in a
    /// stage whose work is complete.
    async fn append_command_event(&self, task_id: &str, payload: Value) {
        match events::append_for_task(&self.pool, task_id, EventType::ShellOutput, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, %err,
                "failed to record stage command output event"
            ),
        }
    }

    /// Applies a finished shell stage's outcome. Nothing is left to return
    /// it to — this runs detached — so a failure is logged and the task
    /// parks in its current stage.
    async fn finish_shell_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        outcome: &str,
        capture: Option<Value>,
    ) {
        match self
            .advance_from_stage(task_id, definition, outcome, Some(stage_name), capture)
            .await
        {
            Ok(()) => tracing::debug!(
                task_id,
                stage = stage_name,
                outcome,
                "shell stage completed; advanced"
            ),
            // Deliberately parked, not broken: the loader guarantees a
            // `done` edge exists, so this is a failed command on a stage
            // that maps no `error` edge on purpose, waiting for a human.
            // Logged at info so it doesn't read as a fault.
            Err(EngineError::UnknownOutcome { stage, outcome }) => tracing::info!(
                task_id,
                stage,
                outcome,
                "shell stage parked: its outcome has no 'on:' edge"
            ),
            // Not reachable from any path today (nothing moves a task out
            // of a shell stage while its command is running), so this is
            // the invariant announcing itself rather than a known case.
            Err(EngineError::StageMovedOn { expected, actual }) => tracing::warn!(
                task_id,
                expected,
                actual,
                outcome,
                "discarded a shell stage's outcome: the task had already left that stage"
            ),
            // Anything else — a transient DB failure in `advance`, say —
            // leaves the task stuck in a stage whose work is already done,
            // with nothing that will retry it. Distinguished from the park
            // above so it doesn't hide among expected outcomes.
            Err(err) => tracing::error!(
                task_id, stage = stage_name, outcome, %err,
                "task wedged: its shell stage completed but the transition failed"
            ),
        }
    }

    /// Starts a `poll` stage's loop (§5.2) and returns immediately.
    ///
    /// Everything that should fail the transition that entered the stage —
    /// where a caller can still see the error — is resolved here rather
    /// than in the detached loop: the task's working directory, and the
    /// `outcomes:` patterns, which are compiled once for the whole stage
    /// instead of per attempt.
    #[allow(clippy::too_many_arguments)]
    async fn enter_poll(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        command: ShellCommand,
        capture: Option<Capture>,
        interval: Duration,
        timeout: Option<Duration>,
        outcomes: &[PollOutcome],
    ) -> Result<(), EngineError> {
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;

        let compiled = poll::compile(outcomes).map_err(|err| EngineError::InvalidPollPattern {
            stage: stage_name.to_string(),
            reason: err.to_string(),
        })?;

        self.spawn_poll_runner(
            task_id.to_string(),
            Arc::clone(definition),
            stage_name.to_string(),
            PollRun {
                command,
                capture,
                interval,
                timeout,
                outcomes: compiled,
                cwd: task_cwd(&task),
            },
        );
        Ok(())
    }

    /// Deliberately a *synchronous* fn, for the same reason
    /// `spawn_shell_runner` is: the spawned future eventually calls
    /// `advance` → `enter_stage` → `enter_poll`, i.e. back to here, and
    /// discharging `tokio::spawn`'s `Send` obligation from inside an
    /// `async fn` would make that cycle part of the compiler's auto-trait
    /// inference for `enter_poll`'s own future and fail to resolve.
    fn spawn_poll_runner(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        run: PollRun,
    ) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine
                .run_poll_stage(&task_id, &definition, &stage_name, run)
                .await;
        });
    }

    /// Runs the command on `interval` until an outcome matches or the
    /// `timeout` budget runs out (§5.2).
    ///
    /// Unlike `shell`, the command's *exit code decides nothing*: a polled
    /// command failing is ordinary — `gh` on a rate limit or a dropped
    /// connection — and is exactly the condition polling exists to ride
    /// out. Only the output is matched. The one failure that does end the
    /// loop is a command that could not be started at all, which no amount
    /// of retrying will fix.
    async fn run_poll_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        run: PollRun,
    ) {
        let described = describe_command(&run.command);
        // Captured once, so a long poll's budget can't drift with the
        // accumulated cost of its own bookkeeping.
        let deadline = run.timeout.map(|limit| std::time::Instant::now() + limit);
        let mut attempt: u64 = 0;
        // What the previous attempt produced, for the "only record what
        // changed" rule below. `None` until the first attempt reports.
        let mut previous: Option<String> = None;

        loop {
            // A poll is the one stage kind that holds its window open for
            // minutes or hours, so unlike `shell` it cannot assume the task
            // is still where it left it. Without this, a poll with no
            // `timeout:` on a task a human has since closed would run
            // forever. This is advisory only — the authoritative check is
            // `advance_from_stage`'s `expected_stage`, taken inside the
            // per-task lock; this just stops the loop early rather than
            // letting it burn a command every interval until the deadline.
            if attempt > 0 && !self.still_in_stage(task_id, stage_name).await {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    attempts = attempt,
                    "abandoned a poll: the task had already left that stage"
                );
                return;
            }

            let remaining =
                deadline.map(|at| at.saturating_duration_since(std::time::Instant::now()));
            if remaining == Some(Duration::ZERO) {
                self.finish_poll_timed_out(
                    task_id, definition, stage_name, &described, attempt, None,
                )
                .await;
                return;
            }

            attempt += 1;
            let started = std::time::Instant::now();
            // Each attempt is capped at whatever is left of the stage's
            // budget rather than at `interval`: a command that legitimately
            // takes longer than its own interval — a slow `gh` call on a
            // 30s poll — would otherwise be killed on every single attempt
            // and the stage could never resolve. With no `timeout:` at all
            // there is no cap, and a hung command parks the task, the same
            // gap `shell` carries without one.
            let outcome = match shell::run(&run.command, &run.cwd, remaining).await {
                Ok(outcome) => outcome,
                // Nothing ran and nothing will: no `sh` on PATH, or a
                // `script_file` that isn't executable. Retrying on an
                // interval would just burn the whole budget to reach the
                // same place, so this ends the poll.
                Err(err @ shell::ShellError::Spawn(_)) => {
                    tracing::error!(
                        task_id, stage = stage_name, %err,
                        "poll stage command could not be started"
                    );
                    self.append_command_event(
                        task_id,
                        json!({
                            "stage": stage_name,
                            "command": described,
                            "attempt": attempt,
                            "exit_code": Value::Null,
                            "timed_out": false,
                            "duration_ms": elapsed_ms(started),
                            "stdout_tail": "",
                            "stderr_tail": "",
                            "note": err.to_string(),
                        }),
                    )
                    .await;
                    self.finish_poll_stage(task_id, definition, stage_name, "error", None)
                        .await;
                    return;
                }
                // The command *did* run — possibly for a long time — and
                // only reading its pipes failed. That says nothing about
                // the state being polled, so keep polling; the attempt
                // simply contributes no output to match against.
                Err(err @ shell::ShellError::Io(_)) => {
                    tracing::warn!(
                        task_id, stage = stage_name, %err,
                        "poll stage command ran but its output could not be read"
                    );
                    let note = err.to_string();
                    self.record_poll_attempt(
                        task_id,
                        &mut previous,
                        // Prefixed with a NUL so an I/O failure can never
                        // collide with a command that happens to print the
                        // same text, which would suppress the event.
                        format!("\0io:{note}"),
                        json!({
                            "stage": stage_name,
                            "command": described,
                            "attempt": attempt,
                            "exit_code": Value::Null,
                            "timed_out": false,
                            "duration_ms": elapsed_ms(started),
                            "stdout_tail": "",
                            "stderr_tail": "",
                            "note": note,
                        }),
                    )
                    .await;
                    if self
                        .sleep_before_next_attempt(run.interval, deadline)
                        .await
                        .is_break()
                    {
                        self.finish_poll_timed_out(
                            task_id, definition, stage_name, &described, attempt, None,
                        )
                        .await;
                        return;
                    }
                    continue;
                }
            };

            if let Some(matched) = run.outcomes.matching(&outcome.stdout) {
                let (captured, capture_note) =
                    derive_capture(run.capture, &outcome.stdout, task_id, stage_name);
                let mut note = format!("matched \"{}\" on attempt {attempt}", matched.pattern);
                if let Some(capture_note) = capture_note {
                    note.push_str("; ");
                    note.push_str(&capture_note);
                }
                // A match can still come from an attempt that was killed at
                // the budget's edge — the output it printed before dying is
                // real and worth honouring — but if its process group
                // outlived the kill, that outranks everything else here:
                // the workflow is about to move on while something may
                // still be running in the same working copy. Same
                // precedence `run_shell_stage` gives it.
                if outcome.escaped {
                    note = "could not confirm the process group exited — something may still be running"
                        .to_string();
                }
                // Always recorded, changed output or not: this is the
                // attempt that decided the stage.
                self.append_command_event(
                    task_id,
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "attempt": attempt,
                        "exit_code": outcome.exit_code,
                        "timed_out": outcome.timed_out,
                        "escaped": outcome.escaped,
                        "duration_ms": duration_ms(outcome.duration),
                        // Queryable siblings of the note, so "which rule
                        // fired?" is answerable from `choco --json`
                        // without matching free text.
                        "matched": matched.pattern,
                        "outcome": matched.then,
                        "stdout_tail": tail(&outcome.stdout),
                        "stderr_tail": tail(&outcome.stderr),
                        "note": note,
                    }),
                )
                .await;
                self.finish_poll_stage(task_id, definition, stage_name, matched.then, captured)
                    .await;
                return;
            }

            // No match. A killed attempt means the budget it was capped at
            // is now spent, so the stage is out of time regardless of what
            // the interval says.
            let killed = outcome.timed_out;
            self.record_poll_attempt(
                task_id,
                &mut previous,
                // Compared trimmed, matching what `tail` puts on the
                // timeline: two attempts differing only in trailing
                // whitespace would otherwise record two entries a reader
                // can't tell apart.
                outcome.stdout.trim().to_string(),
                json!({
                    "stage": stage_name,
                    "command": described,
                    "attempt": attempt,
                    "exit_code": outcome.exit_code,
                    "timed_out": outcome.timed_out,
                    "escaped": outcome.escaped,
                    "duration_ms": duration_ms(outcome.duration),
                    "stdout_tail": tail(&outcome.stdout),
                    "stderr_tail": tail(&outcome.stderr),
                }),
            )
            .await;

            if killed {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    Some(&outcome),
                )
                .await;
                return;
            }

            if self
                .sleep_before_next_attempt(run.interval, deadline)
                .await
                .is_break()
            {
                self.finish_poll_timed_out(
                    task_id, definition, stage_name, &described, attempt, None,
                )
                .await;
                return;
            }
        }
    }

    /// Whether the task is still sitting in the stage this runner belongs
    /// to.
    ///
    /// A read failure answers "yes" on purpose: the alternative is
    /// abandoning a live poll because one `SELECT` failed, which strands a
    /// task nothing will come back to. The error is logged rather than
    /// dropped, and a genuinely departed stage is caught anyway by
    /// `advance_from_stage`'s `expected_stage` check when the poll
    /// eventually reports.
    async fn still_in_stage(&self, task_id: &str, stage_name: &str) -> bool {
        match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => state.current_stage == stage_name,
            // No row at all means the task was deleted underneath us;
            // there is nothing left to poll for.
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, stage = stage_name, %err,
                    "could not confirm a polling task is still in its stage; continuing to poll"
                );
                true
            }
        }
    }

    /// Records an attempt that decided nothing, but only when it said
    /// something new.
    ///
    /// A `gh pr checks` poll at 30s over an hour is 120 attempts printing
    /// the same `PENDING`; one timeline entry per attempt would bury every
    /// other event the task produced, and the retention job prunes by age
    /// alone so nothing else bounds it. Recording only what *changed*
    /// keeps the useful signal — the moment the output flips — while
    /// collapsing the noise, and the first attempt always reports because
    /// it has nothing to be the same as.
    async fn record_poll_attempt(
        &self,
        task_id: &str,
        previous: &mut Option<String>,
        current: String,
        payload: Value,
    ) {
        let changed = previous.as_deref() != Some(current.as_str());
        *previous = Some(current);
        if changed {
            self.append_command_event(task_id, payload).await;
        }
    }

    /// Waits out the interval, or reports that the budget is gone.
    ///
    /// Measured from the end of one attempt to the start of the next
    /// rather than on a fixed cadence, so a command slower than its own
    /// interval can't have attempts overlap and stack up on top of each
    /// other in the task's working copy.
    async fn sleep_before_next_attempt(
        &self,
        interval: Duration,
        deadline: Option<std::time::Instant>,
    ) -> std::ops::ControlFlow<()> {
        let Some(deadline) = deadline else {
            tokio::time::sleep(interval).await;
            return std::ops::ControlFlow::Continue(());
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return std::ops::ControlFlow::Break(());
        }
        // Never sleep past the deadline: a 60s interval under a 70s budget
        // should give up at 70s, not at 120s.
        tokio::time::sleep(interval.min(remaining)).await;
        std::ops::ControlFlow::Continue(())
    }

    /// Ends a poll that ran out of budget with no matching outcome (§5.2's
    /// `on_timeout`). The loader guarantees a `timeout` edge exists
    /// whenever the stage sets a `timeout:`, so this reaches
    /// `finish_poll_stage`'s park path only for a hand-built definition.
    async fn finish_poll_timed_out(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        described: &str,
        attempts: u64,
        last: Option<&shell::ShellOutcome>,
    ) {
        tracing::info!(
            task_id,
            stage = stage_name,
            attempts,
            "poll stage gave up: its timeout elapsed with no matching outcome"
        );

        let mut note = format!("no outcome matched in {attempts} attempts; timeout elapsed");
        // Outranks the plain timeout wording: the workflow is about to
        // follow its `timeout` edge while the last command may still be
        // running in the same working copy.
        if last.is_some_and(|outcome| outcome.escaped) {
            note = "could not confirm the process group exited — something may still be running"
                .to_string();
        }
        self.append_command_event(
            task_id,
            json!({
                "stage": stage_name,
                "command": described,
                "attempt": attempts,
                "exit_code": last.and_then(|outcome| outcome.exit_code),
                "timed_out": true,
                "escaped": last.is_some_and(|outcome| outcome.escaped),
                "duration_ms": last.map_or(0, |outcome| duration_ms(outcome.duration)),
                "stdout_tail": last.map_or_else(String::new, |outcome| tail(&outcome.stdout)),
                "stderr_tail": last.map_or_else(String::new, |outcome| tail(&outcome.stderr)),
                "note": note,
            }),
        )
        .await;

        self.finish_poll_stage(task_id, definition, stage_name, "timeout", None)
            .await;
    }

    /// Applies a finished poll stage's outcome. Nothing is left to return
    /// it to — this runs detached — so a failure is logged and the task
    /// parks in its current stage. Mirrors `finish_shell_stage`, with its
    /// own messages so log aggregation can tell the two kinds apart.
    async fn finish_poll_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        outcome: &str,
        capture: Option<Value>,
    ) {
        match self
            .advance_from_stage(task_id, definition, outcome, Some(stage_name), capture)
            .await
        {
            Ok(()) => tracing::debug!(
                task_id,
                stage = stage_name,
                outcome,
                "poll stage resolved; advanced"
            ),
            // Deliberately parked, not broken: a stage that maps no edge
            // for the outcome it just produced — an `error` with no
            // `on: { error: … }` — is waiting for a human on purpose.
            Err(EngineError::UnknownOutcome { stage, outcome }) => tracing::info!(
                task_id,
                stage,
                outcome,
                "poll stage parked: its outcome has no 'on:' edge"
            ),
            // Reachable here in a way it isn't for `shell`: a poll holds
            // its stage open for as long as its budget allows, so a human
            // resuming or closing the task mid-poll really can move it on
            // between the last `still_in_stage` check and this write.
            Err(EngineError::StageMovedOn { expected, actual }) => tracing::info!(
                task_id,
                expected,
                actual,
                outcome,
                "discarded a poll stage's outcome: the task had already left that stage"
            ),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, outcome, %err,
                "task wedged: its poll stage resolved but the transition failed"
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn enter_agent_turn(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        stage_def: &StageDef,
        role: &str,
        prompt_file: Option<&std::path::Path>,
        input: Option<&str>,
    ) -> Result<(), EngineError> {
        // `WorkflowDefinition::parse`/`load` reject an agent_turn stage
        // with an unknown role, but `roles`/`stages` are `pub` fields with
        // no private-construction guard — a definition built by hand
        // (struct literal) rather than through those constructors could
        // reach here unvalidated, so this stays a reported error rather
        // than an `.expect()` (§ review on PR #35).
        let role_def = definition
            .roles
            .get(role)
            .ok_or_else(|| EngineError::UnknownRole {
                stage: stage_name.to_string(),
                role: role.to_string(),
            })?;

        let prompt = match prompt_file {
            Some(path) => fs::read_to_string(path).map_err(EngineError::Io)?,
            None => input
                .ok_or_else(|| EngineError::MissingAgentTurnInput(stage_name.to_string()))?
                .to_string(),
        };

        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let global = self
            .load_global_config()
            .map_err(EngineError::GlobalConfig)?;
        let resolved = role_config::resolve(role, role_def, &global, &task.config, task_cwd(&task))
            .map_err(EngineError::RoleConfig)?;

        let task_run = task_runs::create(
            &self.pool,
            task_runs::NewTaskRun {
                task_id,
                stage: stage_name,
                role,
                cli_adapter: &resolved.cli,
                model: &resolved.model,
            },
        )
        .await?;

        // Recorded *before* starting the session, not after — once
        // started, the drain task can react and append its own events
        // (session_meta, the reply) at any point, on any thread, so
        // recording first is what guarantees this event always sorts
        // ahead of anything the session produces, regardless of
        // scheduling (see `send_message`'s identical reasoning). Only
        // when `prompt` came from human-typed `input`, not a
        // `prompt_file` — a template-rendered system prompt (a
        // coder/reviewer turn's own instructions, say) isn't something a
        // human said, so it doesn't belong in the human side of the
        // conversation the way a chat task's initial message does.
        // Best-effort: a transient DB failure here shouldn't block
        // starting the turn.
        if prompt_file.is_none()
            && let Err(err) = events::append(
                &self.pool,
                &task_run.id,
                EventType::HumanMessage,
                json!({ "text": prompt }),
            )
            .await
        {
            tracing::error!(task_run_id = %task_run.id, %err, "failed to record human message event");
        }

        if let Err(err) = self
            .session_manager
            .start(&task_run.id, &prompt, &resolved.role_config)
            .await
        {
            // The task_run row was just created `Active` above; without
            // this, a spawn failure here leaves it Active forever (nothing
            // else in this module ever transitions it), wedging the task
            // since workflow_state was already committed to this stage by
            // the caller before enter_stage ran (§ review on PR #35).
            //
            // This still leaves the *task* itself — as opposed to this
            // task_run — with no queryable "stuck" signal beyond this log
            // line and `end_reason: "start_failed"` on the task_run:
            // nothing here marks `workflow_state`/`tasks` in a way an
            // operator or API layer could discover without already knowing
            // to look. Acknowledged gap for Phase 1; surfacing it (e.g. a
            // task status or a query joining `tasks` to a stalled
            // `task_run`) is expected to land with a dedicated follow-up,
            // not silently absorbed here.
            tracing::error!(task_id, task_run_id = %task_run.id, %err, "failed to start session for agent_turn");
            if let Err(update_err) = task_runs::update_status(
                &self.pool,
                &task_run.id,
                TaskRunStatus::Exited,
                Some(Utc::now()),
                Some(TaskRunEndReason::StartFailed),
            )
            .await
            {
                tracing::error!(
                    task_run_id = %task_run.id, %update_err,
                    "failed to mark task run exited after a failed session start"
                );
            }
            return Err(EngineError::Session(err));
        }

        // A stage with an empty `on:` map (chat, §5.4) never concludes —
        // it just keeps accepting further live messages into the same
        // session indefinitely, so there's no outcome to ever watch for.
        if !stage_def.on.is_empty() {
            self.spawn_turn_watcher(task_id.to_string(), Arc::clone(definition), task_run.id);
        }
        Ok(())
    }

    /// Watches a single-shot `agent_turn`'s `task_run` for completion and
    /// auto-advances with the outcome a plain turn emits (§5.2: "a plain
    /// single-shot turn just emits `done`"). A crashed/non-zero exit is
    /// logged and left for a human to notice rather than guessing an
    /// outcome the stage's `on:` map was never designed to receive.
    fn spawn_turn_watcher(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        task_run_id: String,
    ) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match task_runs::get(&engine.pool, &task_run_id).await {
                    // `Idle` is also what the idle reaper leaves behind
                    // when it force-closes a stalled turn's stdin
                    // (session.rs's `drain_session`) — indistinguishable
                    // from a turn finishing on its own by `status` alone,
                    // so `end_reason` is what actually decides whether
                    // this was a real completion.
                    Ok(Some(run))
                        if run.status == TaskRunStatus::Idle
                            && run.end_reason == Some(TaskRunEndReason::Reaped) =>
                    {
                        tracing::warn!(
                            task_id,
                            task_run_id,
                            "task run was force-closed by the idle reaper before completing its turn; not auto-advancing"
                        );
                        return;
                    }
                    Ok(Some(run)) if run.status == TaskRunStatus::Idle => break,
                    Ok(Some(run)) if run.status == TaskRunStatus::Exited => {
                        tracing::warn!(
                            task_id,
                            task_run_id,
                            "task run exited without completing its turn cleanly; not auto-advancing"
                        );
                        return;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        tracing::error!(
                            task_id,
                            task_run_id,
                            "task run disappeared while watching for turn completion; not auto-advancing"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::error!(
                            task_id, task_run_id, %err,
                            "failed to poll task run while watching for turn completion; not auto-advancing"
                        );
                        return;
                    }
                }
                tokio::time::sleep(TURN_WATCH_INTERVAL).await;
            }
            if let Err(err) = engine.advance(&task_id, &definition, "done").await {
                tracing::error!(task_id, %err, "failed to auto-advance task on turn completion");
            } else {
                tracing::debug!(
                    task_id,
                    task_run_id,
                    "turn completed; auto-advanced with \"done\""
                );
            }
        });
    }
}

/// A task's working directory for its agent subprocess: `task.config.cwd`
/// if set, else the daemon's own current directory, else (only if that
/// fails too) an empty path. Shared by `enter_agent_turn` and
/// `send_message` — both need the same task-wide (not per-role) value
/// alongside `role_config::resolve`'s per-role fields.
fn task_cwd(task: &Task) -> PathBuf {
    task.config
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// Largest captured value stored in `workflow_state.payload`, *per stage* —
/// a workflow with several capturing stages can hold a multiple of this.
/// The payload is rewritten in full on every transition for the rest of the
/// task's life, so an unbounded capture from one chatty command would be
/// paid for again on every subsequent hop. Output past this cap isn't
/// captured at all — silently truncating it would hand a later stage a
/// value that looks whole but isn't.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// How much of a command's stdout/stderr goes onto the timeline. Small on
/// purpose: this is a human-facing breadcrumb, and the full output isn't
/// retained anywhere.
const EVENT_OUTPUT_TAIL_BYTES: usize = 2048;

/// Everything a detached poll runner needs, resolved once on stage entry.
///
/// A struct rather than eight parameters threaded through three functions:
/// the trio hands this straight down untouched, and `outcomes` in
/// particular must be compiled exactly once for the whole stage rather
/// than per attempt.
struct PollRun {
    command: ShellCommand,
    capture: Option<Capture>,
    interval: Duration,
    timeout: Option<Duration>,
    outcomes: poll::CompiledOutcomes,
    cwd: PathBuf,
}

/// A duration as whole milliseconds, saturating rather than wrapping — a
/// nonsense number on the timeline is worse than a clamped one.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_ms(since: std::time::Instant) -> u64 {
    duration_ms(since.elapsed())
}

/// The trailing `EVENT_OUTPUT_TAIL_BYTES` of `text`, trimmed. The *tail*
/// rather than the head because a failing command's actual error is
/// almost always the last thing it printed.
fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= EVENT_OUTPUT_TAIL_BYTES {
        return trimmed.to_string();
    }
    // Walk back to a char boundary so a multi-byte char isn't split.
    let mut start = trimmed.len() - EVENT_OUTPUT_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

/// How a command is named on the timeline. A `script_file` shows its path;
/// there's no meaningful "command line" to display for one.
fn describe_command(command: &ShellCommand) -> String {
    match command {
        ShellCommand::Inline(line) => line.clone(),
        ShellCommand::ScriptFile(path) => path.display().to_string(),
    }
}

/// Turns a finished command's stdout into the value stored under
/// `payload.stages.<stage>`, plus an optional note for the timeline when
/// something about that needed explaining.
///
/// A stage with no `capture:` stores nothing at all — only a stage that
/// asked for its output to be kept gets a payload entry.
///
/// Unparseable JSON under `capture: json` is deliberately *not* an error:
/// §5.2 makes the exit code the only thing that decides a shell stage's
/// outcome, so the stdout is kept as text and the stage still reports what
/// its exit code said. The note is what tells a reader why the value isn't
/// the object they expected.
fn derive_capture(
    capture: Option<Capture>,
    stdout: &str,
    task_id: &str,
    stage_name: &str,
) -> (Option<Value>, Option<String>) {
    let Some(capture) = capture else {
        return (None, None);
    };

    let trimmed = stdout.trim();
    if trimmed.len() > MAX_CAPTURE_BYTES {
        tracing::warn!(
            task_id,
            stage = stage_name,
            bytes = trimmed.len(),
            "shell stage output too large to capture; not stored"
        );
        return (
            None,
            Some(format!(
                "output not captured: {} bytes exceeds the {MAX_CAPTURE_BYTES}-byte limit",
                trimmed.len()
            )),
        );
    }

    match capture {
        Capture::Text => (Some(Value::String(trimmed.to_string())), None),
        Capture::Json => match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => (Some(value), None),
            Err(err) => {
                tracing::warn!(
                    task_id, stage = stage_name, %err,
                    "shell stage output was not valid JSON; captured as text"
                );
                (
                    Some(Value::String(trimmed.to_string())),
                    Some(format!(
                        "stdout was not valid JSON ({err}); captured as text"
                    )),
                )
            }
        },
    }
}

/// Stores a stage's captured stdout at `payload.stages.<stage>` (§5.1).
///
/// The `stages` namespace is explicit rather than the payload root because
/// `payload` is one shared blob for the whole task — reserving a top-level
/// key leaves room for other engine-owned namespaces later without a
/// migration, and without a workflow whose stage is *named* `stages`
/// colliding with one. The path matches the `{{ stages.<name>.<field> }}`
/// templating P2-3 will resolve against it.
///
/// Re-entering a stage overwrites its previous capture: the value means
/// "what this stage produced most recently", which is what a later stage
/// templating it wants. The stage trail on the events timeline is what
/// records that it ran more than once.
fn merge_stage_capture(payload: &mut Value, stage: &str, value: Value) {
    // A payload that isn't an object (hand-edited row, or a shape some
    // future writer chose) would silently swallow the capture if this
    // just gave up, so replace it — the engine owns this column, and
    // nothing else writes it today.
    if !payload.is_object() {
        *payload = json!({});
    }
    let stages = payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .entry("stages")
        .or_insert_with(|| json!({}));
    if !stages.is_object() {
        *stages = json!({});
    }
    stages
        .as_object_mut()
        .expect("stages was just ensured to be an object")
        .insert(stage.to_string(), value);
}

/// Increments the guarded stage's transition count. Seeds a fresh entry
/// with `entered_from: stage` (rather than e.g. `null`) so that the
/// `note_stage_entry` call later in the same `advance()` — which, for a
/// stage whose guarded outcome loops back to itself, targets this exact
/// entry — sees a match and doesn't immediately reset the count this
/// call just produced.
fn bump_loop_counter(loop_counters: &mut Value, stage: &str) -> u64 {
    let obj = loop_counters
        .as_object_mut()
        .expect("engine always stores loop_counters as a JSON object");
    let entry = obj
        .entry(stage.to_string())
        .or_insert_with(|| json!({ "entered_from": stage, "count": 0 }));
    let count = entry.get("count").and_then(Value::as_u64).unwrap_or(0) + 1;
    entry["count"] = json!(count);
    count
}

/// Zeroes a guarded stage's count after it reroutes to `then:`, so the
/// same loop can run again later without staying permanently maxed out.
fn reset_loop_count(loop_counters: &mut Value, stage: &str) {
    if let Some(entry) = loop_counters.get_mut(stage) {
        entry["count"] = json!(0);
    }
}

/// Records which stage `stage` was just entered from, resetting its
/// guard count if that differs from last time (§5.3's reset condition).
/// A no-op for stages without a `loop_guard` — there's nothing to track.
fn note_stage_entry(
    loop_counters: &mut Value,
    definition: &WorkflowDefinition,
    stage: &str,
    entered_from: &str,
) {
    let Some(stage_def) = definition.stages.get(stage) else {
        return;
    };
    if stage_def.loop_guard.is_none() {
        return;
    }
    let obj = loop_counters
        .as_object_mut()
        .expect("engine always stores loop_counters as a JSON object");
    let recorded_from = obj
        .get(stage)
        .and_then(|entry| entry.get("entered_from"))
        .and_then(Value::as_str);
    if recorded_from != Some(entered_from) {
        obj.insert(
            stage.to_string(),
            json!({ "entered_from": entered_from, "count": 0 }),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration as StdDuration;

    use serde_json::json;

    use super::*;
    use crate::adapter::{AgentAdapter, ClaudeAdapter};
    use crate::db::{connect_in_memory, projects, tasks};

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    async fn seed_task(pool: &SqlitePool, workflow_def: &str) -> String {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def,
                title: "T",
                config: json!({}),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn engine_with_adapter(pool: SqlitePool, binary: &str) -> Arc<WorkflowEngine> {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let events_notify = Arc::new(Notify::new());
        let session_manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
        );
        // No test here drives create_task/send_message's workflow-name
        // resolution or a global config file, so an inert directory and
        // no config path are enough — role_config::resolve just falls
        // through to whatever the hand-built definition's `roles:` block
        // already specifies, exactly like before this field existed.
        WorkflowEngine::new(
            pool,
            session_manager,
            PathBuf::from("."),
            None,
            events_notify,
        )
    }

    fn engine_with_adapter_and_workflows_dir(
        pool: SqlitePool,
        binary: &str,
        workflows_dir: &Path,
    ) -> Arc<WorkflowEngine> {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let events_notify = Arc::new(Notify::new());
        let session_manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
        );
        WorkflowEngine::new(
            pool,
            session_manager,
            workflows_dir.to_path_buf(),
            None,
            events_notify,
        )
    }

    /// Waits for `workflow_state.current_stage` to reach `expected`.
    ///
    /// Note what this does *not* tell you: `advance` writes `current_stage`
    /// before calling `enter_stage`, so this goes true while the stage's own
    /// effects — the `stage_entered` event, and `update_status("closed")`
    /// for a terminal stage — are still pending. A test asserting on those
    /// must wait for them directly (see [`wait_until_task_status`]) rather
    /// than treating arrival at the stage as proof they already happened.
    async fn wait_until_stage(pool: &SqlitePool, task_id: &str, expected: &str) {
        for _ in 0..500 {
            let state = workflow_state::get(pool, task_id).await.unwrap().unwrap();
            if state.current_stage == expected {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for stage {expected}");
    }

    /// Waits for `tasks.status`, which a terminal stage sets from inside
    /// `enter_stage` — strictly after `current_stage` already names that
    /// stage. Budgeted for a loaded parallel suite: these tests drive a real
    /// python subprocess, and spawning one can take well over a second when
    /// the whole workspace is running at once.
    async fn wait_until_task_status(pool: &SqlitePool, task_id: &str, expected: &str) {
        let mut last = String::new();
        for _ in 0..500 {
            last = tasks::get(pool, task_id).await.unwrap().unwrap().status;
            if last == expected {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for status {expected}, last saw {last:?}");
    }

    /// Polls `task_run_id`'s events for one whose `payload.text` equals
    /// `text` (e.g. an assistant reply from the fake-claude fixture),
    /// since event persistence happens on a spawned background task.
    async fn wait_until_events_contain(pool: &SqlitePool, task_run_id: &str, text: &str) {
        for _ in 0..200 {
            let events = crate::db::events::list_for_task_run(pool, task_run_id)
                .await
                .unwrap();
            if events
                .iter()
                .any(|e| e.payload.get("text").and_then(Value::as_str) == Some(text))
            {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for an event with text {text:?}");
    }

    /// A task's stage trail, read back off the events timeline. This is
    /// what replaced `workflow_state.stage_history` (X-3), and it records
    /// strictly more: a timestamp, the outcome that caused each transition,
    /// and — unlike the old column — the entry stage itself.
    async fn stage_trail(pool: &SqlitePool, task_id: &str) -> Vec<(String, Value)> {
        events::list_for_task(pool, task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::StageEntered)
            .map(|e| {
                (
                    e.payload["stage"].as_str().unwrap().to_string(),
                    e.payload["outcome"].clone(),
                )
            })
            .collect()
    }

    fn human_gate_chain_def() -> Arc<WorkflowDefinition> {
        // gate --(resumed)--> done (terminal). No filesystem references,
        // so it can be built directly without a temp dir.
        let yaml = r#"
name: gated
stages:
  gate:
    kind: human_gate
    on: { resumed: done }
  done:
    kind: terminal
"#;
        Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
    }

    fn self_loop_guard_def() -> Arc<WorkflowDefinition> {
        let yaml = r#"
name: guarded
stages:
  a:
    kind: human_gate
    on: { resumed: a }
    loop_guard: { on: resumed, max: 3, then: done }
  done:
    kind: terminal
"#;
        Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
    }

    fn coder_reviewer_guard_def() -> Arc<WorkflowDefinition> {
        // coding <-> internal_review loop guarded on changes_requested,
        // escalating to a human_gate after 3 round trips (mirrors §5.1's
        // coding-task.yaml, minus the shell/poll stages that come in P2).
        let yaml = r#"
name: coder-reviewer
stages:
  coding:
    kind: human_gate
    on: { resumed: internal_review }
  internal_review:
    kind: human_gate
    on:
      approved: done
      changes_requested: coding
    loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }
  escalate_to_human:
    kind: human_gate
    on: { resumed: coding }
  done:
    kind: terminal
"#;
        Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
    }

    #[tokio::test]
    async fn start_task_creates_workflow_state_at_the_entry_stage() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "gate");

        // The entry stage is recorded too (X-3) — the old
        // `stage_history` column only ever appended a stage on the way
        // *out*, so a task's starting stage was never in the trail.
        assert_eq!(
            stage_trail(&pool, &task_id).await,
            vec![("gate".to_string(), Value::Null)]
        );

        // `gate` is a human_gate, so no task_run exists to attribute this
        // to — the case the old schema could not store at all.
        let recorded = events::list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].task_run_id, None);
        assert_eq!(recorded[0].task_id, task_id);
    }

    #[tokio::test]
    async fn advance_transitions_through_the_on_map_and_records_the_trail() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");

        // One entry per stage entered, each carrying the outcome that
        // selected it. `done` is terminal and still gets recorded.
        assert_eq!(
            stage_trail(&pool, &task_id).await,
            vec![
                ("gate".to_string(), Value::Null),
                ("done".to_string(), json!("resumed")),
            ]
        );
    }

    #[tokio::test]
    async fn entering_a_terminal_stage_closes_the_task() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "closed");
    }

    #[tokio::test]
    async fn entering_a_terminal_stage_evicts_its_task_lock() {
        // A terminal task can never be advance()d or start_task'd again,
        // so its task_locks entry should be reclaimed rather than sitting
        // in the map for the rest of the daemon's life (§ review on PR
        // #35).
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let locks = engine.task_locks.lock().await;
        assert!(!locks.contains_key(&task_id));
    }

    #[tokio::test]
    async fn advance_evicts_its_task_lock_even_when_it_errors() {
        // Regression test for the review on PR #35: `task_locks` used to
        // be evicted only on a successful terminal close, so an ordinary
        // caller error (unknown outcome, unknown role, missing prompt
        // file, etc.) left the entry leaked forever. Evicting on *any*
        // error is safe here since it either precedes any write or
        // follows one that already durably committed.
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine
            .advance(&task_id, &def, "nonexistent")
            .await
            .unwrap_err();

        let locks = engine.task_locks.lock().await;
        assert!(!locks.contains_key(&task_id));
    }

    #[tokio::test]
    async fn start_task_evicts_its_task_lock_when_entering_the_stage_fails() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        // No prompt_file on the stage and no initial_input supplied here
        // -> MissingAgentTurnInput, an enter_stage failure after
        // workflow_state::create already committed.
        engine.start_task(&task_id, &def, None).await.unwrap_err();

        let locks = engine.task_locks.lock().await;
        assert!(!locks.contains_key(&task_id));
    }

    /// Regression test for the review on PR #35: eviction used to remove
    /// a task's `task_locks` entry unconditionally, even while another
    /// overlapping caller still held a clone of the same `Arc<Mutex<()>>`
    /// (e.g. blocked waiting on it). A brand-new caller arriving after
    /// that eviction would then get a fresh, unrelated lock — letting it
    /// run concurrently with the still-in-flight holder of the old one,
    /// exactly the lost-update race `task_locks` exists to prevent. A
    /// real 3-way `tokio::spawn` race reproducing this would be
    /// timing-dependent and potentially flaky, so this drives
    /// `evict_task_lock_if_unshared` directly instead: deterministic, and
    /// it's the exact primitive responsible for correctness here.
    #[tokio::test]
    async fn evict_task_lock_if_unshared_skips_eviction_while_another_caller_holds_a_clone() {
        let pool = connect_in_memory().await.unwrap();
        let engine = engine_with_adapter(pool.clone(), "unused");
        let task_id = "task-under-test";

        let lock = engine.lock_for_task(task_id).await;
        // Simulates a second overlapping caller that already fetched the
        // same Arc from the map before this eviction attempt runs.
        let other_callers_clone = engine.lock_for_task(task_id).await;

        engine.evict_task_lock_if_unshared(task_id, &lock).await;
        assert!(
            engine.task_locks.lock().await.contains_key(task_id),
            "must not evict while another caller still references the lock"
        );

        // Once the other caller's reference is gone, this is the sole
        // remaining holder, and eviction proceeds.
        drop(other_callers_clone);
        engine.evict_task_lock_if_unshared(task_id, &lock).await;
        assert!(!engine.task_locks.lock().await.contains_key(task_id));
    }

    /// Regression test for the review on PR #35: `task_locks` exists
    /// specifically to serialize `advance()` so racing callers can't
    /// clobber each other's `workflow_state` read-modify-write. Fires more
    /// concurrent "resumed" calls than the loop_guard's `max` (3) allows,
    /// on real OS threads (`flavor = "multi_thread"`, not just interleaved
    /// `.await` points within one thread) — without the lock, two callers
    /// could both read `count: 0` and both write `count: 1`, losing an
    /// increment and never escalating past the guard.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_advance_calls_on_the_same_task_do_not_lose_updates() {
        let pool = connect_in_memory().await.unwrap();
        let def = self_loop_guard_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let def = Arc::clone(&def);
            let task_id = task_id.clone();
            handles.push(tokio::spawn(async move {
                engine.advance(&task_id, &def, "resumed").await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // 4 real transitions through "resumed" against a guard allowing 3:
        // if any pair of concurrent calls lost an update, the count would
        // fall short and the task would still be looping on "a" instead of
        // having escalated to "done".
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
    }

    /// Regression test for the review on PR #35's eviction-race finding:
    /// mixes real concurrent `advance()` calls that succeed with ones that
    /// error (an outcome absent from every stage's `on:` map), on real OS
    /// threads, to prove `evict_task_lock_if_unshared`'s guard holds under
    /// actual scheduling nondeterminism — not just in the deterministic
    /// single-threaded reproduction of the primitive above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_advance_calls_mixing_errors_and_successes_do_not_lose_updates() {
        let pool = connect_in_memory().await.unwrap();
        let def = self_loop_guard_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let def = Arc::clone(&def);
            let task_id = task_id.clone();
            handles.push(tokio::spawn(async move {
                engine.advance(&task_id, &def, "resumed").await
            }));
        }
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let def = Arc::clone(&def);
            let task_id = task_id.clone();
            handles.push(tokio::spawn(async move {
                engine.advance(&task_id, &def, "bogus-outcome").await
            }));
        }

        let (mut ok_count, mut err_count) = (0, 0);
        for handle in handles {
            match handle.await.unwrap() {
                Ok(()) => ok_count += 1,
                Err(_) => err_count += 1,
            }
        }

        // "bogus-outcome" is never in any stage's `on:` map, so it always
        // errors regardless of interleaving (UnknownOutcome on "a",
        // TerminalStageHasNoTransitions once escalated to "done") — these
        // counts are deterministic even though the interleaving isn't.
        assert_eq!(ok_count, 4);
        assert_eq!(err_count, 4);

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");

        // The task is terminal and every caller has finished: nothing
        // should still be holding this lock, erroring or not.
        assert!(!engine.task_locks.lock().await.contains_key(&task_id));
    }

    #[tokio::test]
    async fn advancing_a_terminal_stage_is_rejected() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let err = engine.advance(&task_id, &def, "resumed").await.unwrap_err();
        assert!(matches!(
            err,
            EngineError::TerminalStageHasNoTransitions(stage) if stage == "done"
        ));
    }

    #[tokio::test]
    async fn advance_rejects_an_outcome_not_in_the_current_stages_on_map() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        let err = engine
            .advance(&task_id, &def, "nonexistent")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::UnknownOutcome { stage, outcome }
                if stage == "gate" && outcome == "nonexistent"
        ));
    }

    #[tokio::test]
    async fn loop_guard_reroutes_after_max_transitions_through_the_guarded_outcome() {
        let pool = connect_in_memory().await.unwrap();
        let def = self_loop_guard_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        // Transitions 1-3 through "resumed" stay on "a" (max: 3 allows
        // three passes); the 4th reroutes to "done".
        for _ in 0..3 {
            engine.advance(&task_id, &def, "resumed").await.unwrap();
            let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
            assert_eq!(state.current_stage, "a");
        }
        engine.advance(&task_id, &def, "resumed").await.unwrap();
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
    }

    #[tokio::test]
    async fn a_loop_guard_reroute_records_the_outcome_that_tripped_the_guard() {
        let pool = connect_in_memory().await.unwrap();
        let def = self_loop_guard_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        for _ in 0..4 {
            engine.advance(&task_id, &def, "resumed").await.unwrap();
        }

        // Pins the semantics of `outcome` when a `loop_guard` overrides the
        // destination. The recorded outcome is the one that *triggered* the
        // transition ("resumed"), not a synthetic name for the guard — even
        // though on the last hop `on["resumed"]` is `a` while the task
        // actually landed on the guard's `then` (`done`).
        //
        // So a consumer reconstructing the trail against the definition
        // can't read the final hop off the `on:` map alone; it has to also
        // consult `loop_guard.then`, which is where the definition already
        // says that redirect lives. Recording a fabricated outcome instead
        // would be worse — it would name an `on:` key that doesn't exist.
        assert_eq!(
            stage_trail(&pool, &task_id).await,
            vec![
                ("a".to_string(), Value::Null),
                ("a".to_string(), json!("resumed")),
                ("a".to_string(), json!("resumed")),
                ("a".to_string(), json!("resumed")),
                ("done".to_string(), json!("resumed")),
            ]
        );
    }

    #[tokio::test]
    async fn loop_guard_count_resets_after_rerouting_so_the_loop_can_run_again() {
        let pool = connect_in_memory().await.unwrap();
        let def = coder_reviewer_guard_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        // 4 round trips through changes_requested escalates to a human
        // (3 allowed + the reroute on the 4th).
        for _ in 0..4 {
            engine.advance(&task_id, &def, "resumed").await.unwrap();
            engine
                .advance(&task_id, &def, "changes_requested")
                .await
                .unwrap();
        }
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "escalate_to_human");

        // Escalation resumes back into the same coding/review loop; the
        // guard should allow another 3 round trips before escalating
        // again, rather than staying permanently tripped.
        engine.advance(&task_id, &def, "resumed").await.unwrap(); // -> coding
        for _ in 0..3 {
            engine.advance(&task_id, &def, "resumed").await.unwrap();
            engine
                .advance(&task_id, &def, "changes_requested")
                .await
                .unwrap();
            let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
            assert_eq!(state.current_stage, "coding");
        }
        engine.advance(&task_id, &def, "resumed").await.unwrap();
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "escalate_to_human");
    }

    /// Two ways into the guarded stage ("start" and "coding"), so
    /// re-entering it from a different prior stage than last time can be
    /// exercised through real transitions rather than by hand-editing
    /// `workflow_state`.
    fn two_paths_into_guarded_stage_def() -> Arc<WorkflowDefinition> {
        let yaml = r#"
name: two-paths
stages:
  start:
    kind: human_gate
    on: { go: review }
  coding:
    kind: human_gate
    on: { resumed: review }
  review:
    kind: human_gate
    on:
      changes_requested: coding
      approved: done
    loop_guard: { on: changes_requested, max: 5, then: escalate }
  escalate:
    kind: terminal
  done:
    kind: terminal
"#;
        Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
    }

    #[tokio::test]
    async fn loop_guard_count_resets_when_the_guarded_stage_is_entered_from_elsewhere() {
        let pool = connect_in_memory().await.unwrap();
        let def = two_paths_into_guarded_stage_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        // start -> review (entered_from "start"), then one round trip
        // through coding back into review (entered_from "coding" now —
        // a different prior stage than last time review was entered).
        engine.advance(&task_id, &def, "go").await.unwrap();
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "review");
        assert_eq!(
            state.loop_counters["review"]["entered_from"],
            json!("coding")
        );

        // If the reset hadn't happened, this would be the guard's 2nd
        // transition (carried over from the "start"-sourced visit); with
        // the reset, it's the 1st against the new "coding"-sourced streak.
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.loop_counters["review"]["count"], json!(1));
    }

    #[tokio::test]
    async fn agent_turn_without_prompt_file_uses_the_supplied_input() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine
            .start_task(&task_id, &def, Some("hello"))
            .await
            .unwrap();

        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].stage, "chatting");
    }

    #[tokio::test]
    async fn agent_turn_without_prompt_file_or_input_errors() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        let err = engine.start_task(&task_id, &def, None).await.unwrap_err();
        assert!(matches!(err, EngineError::MissingAgentTurnInput(stage) if stage == "chatting"));
    }

    #[tokio::test]
    async fn a_chat_style_agent_turn_never_auto_advances() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine
            .start_task(&task_id, &def, Some("hello"))
            .await
            .unwrap();

        // fake_claude.py stays alive across turns; give the (nonexistent)
        // watcher a moment it would need if one had incorrectly been
        // spawned, then confirm the stage never moved.
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "chatting");
    }

    #[tokio::test]
    async fn a_single_shot_agent_turn_auto_advances_on_completion() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
        let yaml = r#"
name: coding-task
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_oneshot.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "finished").await;
        // Closing the task happens inside `enter_stage`, after
        // `current_stage` is already "finished" — so this has to be waited
        // for, not read once off the back of the stage having changed.
        wait_until_task_status(&pool, &task_id, "closed").await;

        // The auto-advance is visible in the trail too, with the outcome the
        // adapter reported as what carried it into the terminal stage.
        assert_eq!(
            stage_trail(&pool, &task_id).await,
            vec![
                ("coding".to_string(), Value::Null),
                ("finished".to_string(), json!("done")),
            ]
        );
    }

    #[tokio::test]
    async fn a_crashed_single_shot_turn_does_not_auto_advance() {
        // Drives an actually-crashing subprocess (exit code 1, not a
        // hand-seeded row) through the real spawn_turn_watcher path, to
        // confirm the `Exited` branch's "log and don't advance" behavior
        // holds end to end, not just when unit-tested against seeded state
        // (§ review on PR #35).
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
        let yaml = r#"
name: coding-task
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_crash.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();

        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        for _ in 0..200 {
            if task_runs::get(&pool, &runs[0].id)
                .await
                .unwrap()
                .unwrap()
                .status
                == TaskRunStatus::Exited
            {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }

        // Give the watcher a moment it would need if it had incorrectly
        // decided to auto-advance, then confirm it didn't.
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "coding");
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open");
    }

    #[tokio::test]
    async fn a_turn_reaped_by_the_idle_timeout_does_not_auto_advance() {
        // Regression test for the ambiguity the review on PR #35 flagged:
        // both a completed turn and a reaper-force-closed turn land the
        // task_run on `Idle`, so the watcher must consult `end_reason`
        // rather than treating every `Idle` as "done". The task_run is
        // seeded directly as already `Idle`/`reaped` so the watcher's
        // very first poll observes the condition deterministically,
        // rather than racing a real subprocess to get there first.
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        workflow_state::create(&pool, &task_id, "gate")
            .await
            .unwrap();
        let task_run = task_runs::create(
            &pool,
            task_runs::NewTaskRun {
                task_id: &task_id,
                stage: "gate",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        task_runs::update_status(
            &pool,
            &task_run.id,
            TaskRunStatus::Idle,
            None,
            Some(TaskRunEndReason::Reaped),
        )
        .await
        .unwrap();

        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.spawn_turn_watcher(task_id.clone(), Arc::clone(&def), task_run.id.clone());

        tokio::time::sleep(StdDuration::from_millis(150)).await;
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "gate");
    }

    #[tokio::test]
    async fn a_failed_session_start_marks_the_task_run_exited_instead_of_wedging_it() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
        let yaml = r#"
name: coding-task
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        // A binary that can't be spawned at all, so session_manager.start()
        // fails synchronously rather than the process merely crashing
        // after launch.
        let engine = engine_with_adapter(pool.clone(), "/no/such/binary-3f6c9a");

        engine.start_task(&task_id, &def, None).await.unwrap_err();

        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, TaskRunStatus::Exited);
        assert!(runs[0].ended_at.is_some());
    }

    // P2-2 was the last unimplemented kind, so `enter_stage` now executes
    // every kind the loader accepts and `EngineError::UnsupportedStageKind`
    // is gone, along with the test that covered it (which had already
    // narrowed from `shell`+`poll` to `poll` alone at P2-1).
    // `send_message_or_resume` keeps its own `UnsupportedStageKind` — that
    // one is about which stages accept a *human message*, a different
    // question that still has real answers.
    //
    // What that test asserted is now the compiler's job: the `match` in
    // `enter_stage` is exhaustive over `StageKind`, so adding a kind
    // without executing it won't build.

    // ---- shell stage kind (P2-1) ----------------------------------------

    /// A one-shell-stage workflow: `run` executes `command`, then hands off
    /// to a terminal stage on `done` and a human gate on `error`, so a test
    /// can tell the two outcomes apart by where the task ends up.
    fn shell_def(command: &str, extra: &str) -> Arc<WorkflowDefinition> {
        let yaml = format!(
            r#"
name: shell-flow
stages:
  run:
    kind: shell
    command: "{command}"
{extra}
    on: {{ done: finished, error: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        );
        Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap())
    }

    async fn payload_of(pool: &SqlitePool, task_id: &str) -> Value {
        workflow_state::get(pool, task_id)
            .await
            .unwrap()
            .unwrap()
            .payload
    }

    /// Waits for the `shell_output` entry a shell stage records, and
    /// returns its payload.
    async fn wait_until_shell_event(pool: &SqlitePool, task_id: &str) -> Value {
        for _ in 0..500 {
            let found = events::list_for_task(pool, task_id)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.event_type == EventType::ShellOutput);
            if let Some(event) = found {
                return event.payload;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a shell_output event");
    }

    #[tokio::test]
    async fn a_successful_shell_stage_advances_through_done() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("exit 0", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "finished").await;
        wait_until_task_status(&pool, &task_id, "closed").await;
    }

    #[tokio::test]
    async fn a_failing_shell_stage_advances_through_error() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("exit 7", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "failed").await;
        let event = wait_until_shell_event(&pool, &task_id).await;
        assert_eq!(event["exit_code"], json!(7));
        assert_eq!(event["timed_out"], json!(false));
    }

    /// The capture has to land under `stages.<name>` specifically: that's
    /// the path P2-3's `{{ stages.run.number }}` templating will resolve.
    #[tokio::test]
    async fn capture_json_parses_stdout_into_the_stage_payload() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def(
            r#"printf '{\"number\": 42, \"url\": \"http://x\"}'"#,
            "    capture: json",
        );
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["run"]["number"], json!(42));
        assert_eq!(payload["stages"]["run"]["url"], json!("http://x"));
    }

    #[tokio::test]
    async fn capture_text_stores_trimmed_stdout_as_a_string() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def(r#"printf '  hello\n'"#, "    capture: text");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["run"],
            json!("hello")
        );
    }

    /// §5.2 makes the exit code the *only* thing that decides the outcome,
    /// so stdout that isn't the JSON the stage asked for must not turn a
    /// successful command into a failed stage.
    #[tokio::test]
    async fn unparseable_json_still_succeeds_and_is_captured_as_text() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("printf 'not json at all'", "    capture: json");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["run"],
            json!("not json at all")
        );
        let event = wait_until_shell_event(&pool, &task_id).await;
        assert!(
            event["note"]
                .as_str()
                .unwrap_or_default()
                .contains("not valid JSON"),
            "expected an explanatory note, got {event}"
        );
    }

    /// A failed attempt must not overwrite the capture a successful earlier
    /// attempt at the same stage left behind — `stages.<name>` is keyed by
    /// stage, so a retry loop would otherwise poison the value a later
    /// stage templates.
    #[tokio::test]
    async fn a_failed_command_does_not_overwrite_an_earlier_capture() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: retrying
stages:
  run:
    kind: shell
    command: "if [ -f attempted ]; then exit 1; else touch attempted; printf 'good'; fi"
    capture: text
    on: { done: again, error: failed }
  again:
    kind: shell
    command: "true"
    on: { done: run }
  failed:
    kind: terminal
"#;
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": dir.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        // First pass captures "good", loops back, second pass exits 1.
        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "failed").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["run"],
            json!("good"),
            "the failed retry should not have replaced the good capture"
        );
    }

    #[tokio::test]
    async fn a_stage_without_capture_writes_no_payload() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("printf 'ignored output'", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(payload_of(&pool, &task_id).await, json!({}));
    }

    #[tokio::test]
    async fn a_command_that_exceeds_its_timeout_is_killed_and_errors() {
        let pool = connect_in_memory().await.unwrap();
        // `parse_duration` (shared with `poll`) has whole-second
        // granularity, so 1s is the shortest timeout expressible.
        let def = shell_def("sleep 30", "    timeout: 1s");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "failed").await;
        let event = wait_until_shell_event(&pool, &task_id).await;
        assert_eq!(event["timed_out"], json!(true));
        assert_eq!(event["exit_code"], Value::Null);
    }

    #[tokio::test]
    async fn the_command_and_its_output_land_on_the_task_timeline() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("printf 'to-stderr' >&2; exit 2", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "failed").await;

        let event = wait_until_shell_event(&pool, &task_id).await;
        assert_eq!(event["stage"], json!("run"));
        assert!(
            event["command"].as_str().unwrap().contains("to-stderr"),
            "expected the command line, got {event}"
        );
        assert_eq!(event["stderr_tail"], json!("to-stderr"));
        assert!(event["duration_ms"].is_number());
    }

    /// The event belongs to the task, not to any agent session — a shell
    /// stage opens none — so it must carry a null `task_run_id` and still
    /// appear on the task's timeline.
    #[tokio::test]
    async fn the_shell_event_is_task_scoped_with_no_task_run() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("exit 0", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_shell_event(&pool, &task_id).await;

        let event = events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == EventType::ShellOutput)
            .unwrap();
        assert_eq!(event.task_run_id, None);
        assert_eq!(event.task_id, task_id);
        assert!(
            task_runs::list_for_task(&pool, &task_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// `error` is deliberately optional in a shell stage's `on:` map, so a
    /// failed command with nowhere to go parks the task where it is rather
    /// than crashing or inventing a transition.
    #[tokio::test]
    async fn a_failure_with_no_error_edge_parks_the_task() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: no-error-edge
stages:
  run:
    kind: shell
    command: "exit 1"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        // The command ran and was recorded...
        let event = wait_until_shell_event(&pool, &task_id).await;
        assert_eq!(event["exit_code"], json!(1));
        // ...but there was nowhere to go, so the task stays put.
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "run");
        assert_eq!(
            tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
            "open"
        );
    }

    /// A shell stage's outcome goes through the same `advance` as every
    /// other kind's, so `loop_guard` (§5.3) applies to it unchanged.
    #[tokio::test]
    async fn a_shell_failure_counts_against_a_loop_guard() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: guarded-shell
stages:
  run:
    kind: shell
    command: "exit 1"
    on: { done: finished, error: run }
    loop_guard: { on: error, max: 2, then: gave_up }
  gave_up:
    kind: terminal
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        // Retries itself twice, then the guard reroutes it.
        wait_until_stage(&pool, &task_id, "gave_up").await;
        wait_until_task_status(&pool, &task_id, "closed").await;
    }

    #[tokio::test]
    async fn the_command_runs_in_the_tasks_configured_cwd() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("marker-file"), b"x").unwrap();

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let def = shell_def("ls", "    capture: text");
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": dir.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["run"],
            json!("marker-file")
        );
    }

    /// A capture from an earlier stage has to survive later transitions —
    /// it's the whole point of storing it — and must not be clobbered by
    /// the `advance` that moves the task on.
    #[tokio::test]
    async fn an_earlier_stages_capture_survives_later_transitions() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: two-shells
stages:
  first:
    kind: shell
    command: "printf 'one'"
    capture: text
    on: { done: second }
  second:
    kind: shell
    command: "printf 'two'"
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["first"], json!("one"));
        assert_eq!(payload["stages"]["second"], json!("two"));
    }

    /// A `script_file` runs under its own shebang rather than as a shell
    /// string, and its stdout is captured the same way an inline command's
    /// is.
    #[tokio::test]
    async fn a_script_file_stage_runs_and_captures() {
        use std::os::unix::fs::PermissionsExt;

        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let script = dir.join("do-it.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'from script'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let yaml = r#"
name: script-flow
stages:
  run:
    kind: shell
    script_file: do-it.sh
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["run"],
            json!("from script")
        );
    }

    /// A command that can't be started at all still has to move the task —
    /// "couldn't run it" and "ran and failed" are the same thing to the
    /// workflow — and has to say why on the timeline, since nothing else
    /// records it.
    #[tokio::test]
    async fn a_command_that_cannot_start_errors_with_a_reason() {
        use std::os::unix::fs::PermissionsExt;

        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let script = dir.join("not-executable.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();

        let yaml = r#"
name: bad-script
stages:
  run:
    kind: shell
    script_file: not-executable.sh
    on: { done: finished, error: failed }
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: { resumed: finished }
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "failed").await;

        let event = wait_until_shell_event(&pool, &task_id).await;
        assert!(
            event["note"]
                .as_str()
                .unwrap_or_default()
                .contains("failed to start command"),
            "expected a spawn reason, got {event}"
        );
    }

    /// A task busy running a shell command has no session to relay into,
    /// so a human message is rejected (409 at the API layer) rather than
    /// silently dropped.
    #[tokio::test]
    async fn sending_a_message_to_a_running_shell_stage_is_rejected() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: slow-shell
stages:
  run:
    kind: shell
    command: "sleep 30"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        std::fs::write(dir.join("slow-shell.yaml"), yaml).unwrap();
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

        engine.start_task(&task_id, &def, None).await.unwrap();

        let err = engine
            .send_message_or_resume(&task_id, "are you there?")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SendMessageOrResumeError::UnsupportedStageKind(stage) if stage == "run"),
            "got {err:?}"
        );
    }

    /// The whole point of `expected_stage` is to refuse an outcome from a
    /// stage the task has already left. Every other shell test exercises the
    /// path where it matches, so this pins down the path where it doesn't —
    /// the one P2-2's longer-running `poll` is expected to hit.
    #[tokio::test]
    async fn an_outcome_for_a_stage_the_task_has_left_is_discarded() {
        let pool = connect_in_memory().await.unwrap();
        let def = shell_def("exit 0", "");
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let err = engine
            .advance_from_stage(&task_id, &def, "done", Some("run"), Some(json!("late")))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EngineError::StageMovedOn { expected, actual }
                if expected == "run" && actual == "finished"),
            "got {err:?}"
        );
        // Neither the transition nor the capture was applied.
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "finished");
        assert_eq!(state.payload, json!({}));
    }

    #[test]
    fn a_capture_goes_under_the_stages_namespace() {
        let mut payload = json!({});
        merge_stage_capture(&mut payload, "open_pr", json!({"number": 42}));
        assert_eq!(payload, json!({"stages": {"open_pr": {"number": 42}}}));

        // A second stage joins it rather than replacing it.
        merge_stage_capture(&mut payload, "checks", json!("green"));
        assert_eq!(
            payload,
            json!({"stages": {"open_pr": {"number": 42}, "checks": "green"}})
        );

        // Re-entering a stage overwrites just that stage's value.
        merge_stage_capture(&mut payload, "open_pr", json!({"number": 43}));
        assert_eq!(payload["stages"]["open_pr"]["number"], json!(43));
        assert_eq!(payload["stages"]["checks"], json!("green"));
    }

    /// A payload (or a `stages` key) that isn't an object can't be merged
    /// into, and dropping the capture there would lose it silently.
    #[test]
    fn a_capture_replaces_a_non_object_payload_rather_than_vanishing() {
        let mut payload = json!("not an object");
        merge_stage_capture(&mut payload, "run", json!(1));
        assert_eq!(payload, json!({"stages": {"run": 1}}));

        let mut payload = json!({"stages": "also not an object"});
        merge_stage_capture(&mut payload, "run", json!(2));
        assert_eq!(payload, json!({"stages": {"run": 2}}));
    }

    #[test]
    fn an_oversized_capture_is_skipped_with_an_explanation() {
        let huge = "x".repeat(MAX_CAPTURE_BYTES + 1);
        let (captured, note) = derive_capture(Some(Capture::Text), &huge, "task", "run");
        assert!(captured.is_none());
        assert!(note.unwrap().contains("exceeds"));

        // The limit itself is fine.
        let at_limit = "y".repeat(MAX_CAPTURE_BYTES);
        let (captured, note) = derive_capture(Some(Capture::Text), &at_limit, "task", "run");
        assert_eq!(captured, Some(Value::String(at_limit)));
        assert!(note.is_none());
    }

    #[test]
    fn event_output_tails_are_bounded_and_utf8_safe() {
        assert_eq!(tail("  short  "), "short");

        let long = "é".repeat(EVENT_OUTPUT_TAIL_BYTES);
        let tailed = tail(&long);
        assert!(tailed.starts_with('…'));
        // Truncation landed on a char boundary, so the tail is still the
        // same character repeated — no replacement chars, no panic.
        assert!(tailed.trim_start_matches('…').chars().all(|c| c == 'é'));
    }

    #[test]
    fn resolve_workflow_path_only_accepts_a_safe_allowlisted_name() {
        let dir = tempdir();
        std::fs::write(dir.join("chat.yaml"), "irrelevant").unwrap();

        assert!(resolve_workflow_path(&dir, "chat").is_ok());
        assert!(matches!(
            resolve_workflow_path(&dir, "").unwrap_err(),
            ResolveError::InvalidName(_)
        ));
        assert!(matches!(
            resolve_workflow_path(&dir, "../etc/passwd").unwrap_err(),
            ResolveError::InvalidName(_)
        ));
        assert!(matches!(
            resolve_workflow_path(&dir, "chat/../../etc").unwrap_err(),
            ResolveError::InvalidName(_)
        ));
        assert!(matches!(
            resolve_workflow_path(&dir, "does-not-exist").unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn write_chat_workflow(workflows_dir: &Path) {
        std::fs::write(
            workflows_dir.join("chat.yaml"),
            r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn create_task_resolves_the_named_workflow_and_starts_it() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(
                &project_id,
                None,
                "chat",
                "flaky test",
                "hey, look into it",
                json!({}),
            )
            .await
            .unwrap();

        assert_eq!(task.workflow_def, "chat");
        let state = workflow_state::get(&pool, &task.id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "chatting");
        let runs = task_runs::list_for_task(&pool, &task.id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].stage, "chatting");
    }

    #[tokio::test]
    async fn create_task_with_an_unknown_workflow_name_errors() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

        let err = engine
            .create_task(&project_id, None, "ghost", "t", "hi", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CreateTaskError::Resolve(ResolveError::NotFound(name)) if name == "ghost"
        ));
    }

    #[tokio::test]
    async fn create_task_with_a_nonexistent_project_id_is_a_reported_error_not_a_raw_fk_failure() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

        let err = engine
            .create_task("no-such-project", None, "chat", "t", "hi", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CreateTaskError::NoSuchProject(id) if id == "no-such-project"
        ));
    }

    #[tokio::test]
    async fn create_task_with_a_nonexistent_parent_task_id_is_a_reported_error() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

        let err = engine
            .create_task(
                &project_id,
                Some("no-such-task"),
                "chat",
                "t",
                "hi",
                json!({}),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CreateTaskError::NoSuchParentTask(id) if id == "no-such-task"
        ));
    }

    #[tokio::test]
    async fn send_message_reaches_the_live_session_started_by_create_task() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(&project_id, None, "chat", "t", "hello", json!({}))
            .await
            .unwrap();

        engine
            .send_message(&task.id, "actually check the other branch too")
            .await
            .unwrap();

        // fake_claude.py echoes each line it receives as `echo:<text>` in
        // an assistant message event — proves the follow-up reached the
        // same live process this task's create_task call started.
        let runs = task_runs::list_for_task(&pool, &task.id).await.unwrap();
        let mut saw_echo = false;
        for _ in 0..200 {
            let events = crate::db::events::list_for_task_run(&pool, &runs[0].id)
                .await
                .unwrap();
            if events.iter().any(|e| {
                e.payload
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "echo:actually check the other branch too")
            }) {
                saw_echo = true;
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert!(saw_echo, "follow-up message never reached the live session");
    }

    /// Regression test: the `events` table used to only ever hold what the
    /// agent adapter emitted — the human's own side of the conversation
    /// (both the task's initial prompt and every `send_message` relay)
    /// has nowhere to land otherwise. Checks both write sites at once
    /// (`enter_agent_turn`'s initial-input path and `send_message`'s
    /// relay path) and that they interleave in the right order with the
    /// agent's replies.
    #[tokio::test]
    async fn human_messages_are_recorded_as_events_interleaved_with_replies() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(&project_id, None, "chat", "t", "hello", json!({}))
            .await
            .unwrap();

        // Wait for the initial turn's reply before sending the follow-up,
        // so the two round trips can't land out of order.
        let runs = task_runs::list_for_task(&pool, &task.id).await.unwrap();
        wait_until_events_contain(&pool, &runs[0].id, "echo:hello").await;

        engine.send_message(&task.id, "again").await.unwrap();
        wait_until_events_contain(&pool, &runs[0].id, "echo:again").await;

        let events = crate::db::events::list_for_task_run(&pool, &runs[0].id)
            .await
            .unwrap();
        let kinds_and_text: Vec<(String, Option<&str>)> = events
            .iter()
            .map(|e| {
                (
                    e.event_type.to_string(),
                    e.payload.get("text").and_then(Value::as_str),
                )
            })
            .collect();

        // human_message("hello") is recorded before the session even
        // starts (see engine.rs's `enter_agent_turn`), so it always
        // precedes session_meta/the reply — same for the "again" relay
        // against its own reply. The human's own messages now show up in
        // their correct chronological place, not just the agent's replies.
        assert_eq!(
            kinds_and_text,
            vec![
                ("human_message".to_string(), Some("hello")),
                ("session_meta".to_string(), None),
                ("assistant_message".to_string(), Some("echo:hello")),
                ("human_message".to_string(), Some("again")),
                ("assistant_message".to_string(), Some("echo:again")),
            ]
        );
    }

    /// A `prompt_file`-rendered turn's prompt is template/system-authored
    /// content, not something a human typed — it must not be recorded as
    /// a `human_message` event.
    #[tokio::test]
    async fn a_prompt_file_backed_turn_does_not_record_a_human_message_event() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        std::fs::write(workflows_dir.join("prompt.md"), "Do the templated thing.").unwrap();
        std::fs::write(
            workflows_dir.join("templated.yaml"),
            r#"
name: templated
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    prompt_file: prompt.md
    on: {}
"#,
        )
        .unwrap();
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(&project_id, None, "templated", "t", "ignored", json!({}))
            .await
            .unwrap();

        let runs = task_runs::list_for_task(&pool, &task.id).await.unwrap();
        wait_until_events_contain(&pool, &runs[0].id, "echo:Do the templated thing.").await;

        let events = crate::db::events::list_for_task_run(&pool, &runs[0].id)
            .await
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|e| e.event_type == chokofactory_core::models::EventType::HumanMessage),
            "a prompt_file-backed turn should never record a human_message event"
        );
    }

    #[tokio::test]
    async fn send_message_rejects_a_stage_that_can_transition() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        std::fs::write(
            workflows_dir.join("has-outcome.yaml"),
            r#"
name: has-outcome
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: { done: finished }
  finished:
    kind: terminal
"#,
        )
        .unwrap();
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(&project_id, None, "has-outcome", "t", "hello", json!({}))
            .await
            .unwrap();

        let err = engine.send_message(&task.id, "hi again").await.unwrap_err();
        assert!(matches!(
            err,
            SendMessageError::StageNotOpenEnded(stage) if stage == "chatting"
        ));
    }

    #[tokio::test]
    async fn send_message_errors_when_the_open_stage_has_no_task_run_yet() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);

        // workflow_state seeded directly, skipping create_task/start_task
        // (and therefore skipping the task_run it would have created) —
        // simulates a task whose entry stage never actually got entered.
        let task_id = seed_task(&pool, "chat").await;
        workflow_state::create(&pool, &task_id, "chatting")
            .await
            .unwrap();

        let err = engine.send_message(&task_id, "hello?").await.unwrap_err();
        assert!(matches!(
            err,
            SendMessageError::NoOpenRun(stage) if stage == "chatting"
        ));
    }

    #[tokio::test]
    async fn send_message_errors_for_a_nonexistent_task() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);

        let err = engine
            .send_message("no-such-task", "hello?")
            .await
            .unwrap_err();
        assert!(matches!(err, SendMessageError::NoSuchTask));
    }

    #[tokio::test]
    async fn send_message_errors_when_workflow_state_references_an_unknown_stage() {
        // Reachable given this design's "no caching, always re-read from
        // disk" stance (P1-8 LLD §4.5): the workflow file backing a task
        // could be edited to remove a stage after the task already
        // recorded `workflow_state.current_stage` there.
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);

        let task_id = seed_task(&pool, "chat").await;
        workflow_state::create(&pool, &task_id, "ghost-stage")
            .await
            .unwrap();

        let err = engine.send_message(&task_id, "hello?").await.unwrap_err();
        assert!(matches!(
            err,
            SendMessageError::UnknownStage(stage) if stage == "ghost-stage"
        ));
    }

    /// `send_message_or_resume` resolves its workflow definition from disk
    /// (like `send_message`/`create_task`), unlike `advance`/`start_task`
    /// which take an already-loaded `&Arc<WorkflowDefinition>` straight
    /// from the caller — so, unlike this file's other `human_gate_chain_def`
    /// tests, these two need the same YAML actually written to a
    /// `workflows_dir` under the name the seeded task references.
    fn write_human_gate_chain_workflow(workflows_dir: &Path) {
        std::fs::write(
            workflows_dir.join("gated.yaml"),
            r#"
name: gated
stages:
  gate:
    kind: human_gate
    on: { resumed: done }
  done:
    kind: terminal
"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn send_message_or_resume_routes_a_human_gate_stage_to_advance() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_human_gate_chain_workflow(&workflows_dir);
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, "gated").await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine
            .send_message_or_resume(&task_id, "ignored for a human_gate")
            .await
            .unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
    }

    #[tokio::test]
    async fn send_message_or_resume_routes_an_open_agent_turn_to_send_message() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_chat_workflow(&workflows_dir);
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &workflows_dir,
        );

        let task = engine
            .create_task(&project_id, None, "chat", "t", "hello", json!({}))
            .await
            .unwrap();

        engine
            .send_message_or_resume(&task.id, "actually check the other branch too")
            .await
            .unwrap();

        let runs = task_runs::list_for_task(&pool, &task.id).await.unwrap();
        let mut saw_echo = false;
        for _ in 0..200 {
            let events = crate::db::events::list_for_task_run(&pool, &runs[0].id)
                .await
                .unwrap();
            if events.iter().any(|e| {
                e.payload
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "echo:actually check the other branch too")
            }) {
                saw_echo = true;
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert!(saw_echo, "follow-up message never reached the live session");
    }

    #[tokio::test]
    async fn send_message_or_resume_rejects_an_unsupported_stage_kind() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_human_gate_chain_workflow(&workflows_dir);
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, "gated").await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);
        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.advance(&task_id, &def, "resumed").await.unwrap(); // -> "done" (terminal)

        let err = engine
            .send_message_or_resume(&task_id, "too late")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SendMessageOrResumeError::UnsupportedStageKind(stage) if stage == "done"
        ));
    }

    // ---- poll stage kind (P2-2) ------------------------------------------

    /// A one-poll-stage workflow. `outcomes` and any extra stage fields are
    /// injected as raw YAML so each test can shape them; the three
    /// destinations are distinct so a test can tell which edge fired purely
    /// by where the task ends up — `finished` for a match, `stalled` for the
    /// timeout, `failed` for an error.
    ///
    /// `interval: 1s` is the floor the loader allows (`0s` is a busy loop
    /// and is rejected), so multi-attempt tests below cost real seconds.
    fn poll_def(command: &str, extra: &str) -> String {
        format!(
            r#"
name: poll-flow
stages:
  watch:
    kind: poll
    command: "{command}"
    interval: 1s
{extra}
    on: {{ green: finished, red: failed, error: failed, timeout: stalled }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
  stalled:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        )
    }

    fn parsed_poll_def(command: &str, extra: &str) -> Arc<WorkflowDefinition> {
        Arc::new(WorkflowDefinition::parse(&poll_def(command, extra), Path::new(".")).unwrap())
    }

    /// The `outcomes:` block most tests want: `SUCCESS` is green, anything
    /// mentioning failure is red.
    const GREEN_OR_RED: &str = r#"    outcomes:
      - match: "SUCCESS"
        then: green
      - match: "FAILURE|ERROR"
        then: red"#;

    async fn poll_events(pool: &SqlitePool, task_id: &str) -> Vec<Value> {
        events::list_for_task(pool, task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::ShellOutput)
            .map(|e| e.payload)
            .collect()
    }

    /// Waits until a poll has recorded at least one attempt, i.e. its loop
    /// is genuinely running. Not `wait_until_events_contain`, which looks
    /// events up by `task_run_id` — a poll stage opens no session, so its
    /// entries are task-scoped with no run id at all.
    async fn wait_until_poll_attempt_recorded(pool: &SqlitePool, task_id: &str) {
        for _ in 0..600 {
            if !poll_events(pool, task_id).await.is_empty() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a poll attempt to be recorded");
    }

    /// Waits for the entry a poll records when it resolves — the one
    /// carrying a `note`, as opposed to the bare progress entries.
    async fn wait_until_decisive_poll_event(pool: &SqlitePool, task_id: &str) -> Value {
        for _ in 0..600 {
            if let Some(event) = poll_events(pool, task_id)
                .await
                .into_iter()
                .find(|payload| payload.get("note").is_some())
            {
                return event;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a decisive poll event");
    }

    async fn seed_task_in(pool: &SqlitePool, workflow_def: &str, cwd: &Path) -> String {
        let project_id = projects::create(pool, "demo").await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def,
                title: "T",
                config: json!({ "cwd": cwd.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn a_poll_advances_as_soon_as_an_outcome_matches() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def("echo SUCCESS", GREEN_OR_RED);
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "finished").await;
        wait_until_task_status(&pool, &task_id, "closed").await;

        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["attempt"], json!(1));
        assert_eq!(event["outcome"], json!("green"));
        assert_eq!(event["matched"], json!("SUCCESS"));
    }

    /// Declaration order decides, not which pattern the output happens to
    /// satisfy — `FAILURE` here matches only the second rule, so the task
    /// must take the `red` edge rather than falling through.
    #[tokio::test]
    async fn a_poll_takes_the_edge_of_the_outcome_that_matched() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def("echo FAILURE", GREEN_OR_RED);
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "failed").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["outcome"], json!("red"));
    }

    /// The behaviour the whole kind exists for: the command keeps saying
    /// nothing interesting until the state it watches changes, and only
    /// then does the stage move.
    ///
    /// Also pins the timeline policy. Three attempts print `PENDING`,
    /// `PENDING`, `SUCCESS`, and exactly two entries are recorded — the
    /// first `PENDING` (nothing to be the same as) and the decisive
    /// `SUCCESS`. The repeated `PENDING` is what a real `gh pr checks` poll
    /// produces dozens of times, and burying the timeline under it is the
    /// thing this rule prevents.
    #[tokio::test]
    async fn a_poll_keeps_running_until_its_output_changes_and_records_only_the_changes() {
        use std::os::unix::fs::PermissionsExt;

        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        // Counts its own runs through a file in the task's working
        // directory, so this doubles as the check that `cwd` is honoured:
        // with the wrong directory the counter never accumulates and the
        // poll would run to its timeout instead.
        let script = dir.join("check.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nn=$(cat count 2>/dev/null || echo 0)\nn=$((n+1))\n\
             echo $n > count\nif [ $n -ge 3 ]; then echo SUCCESS; else echo PENDING; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let yaml = format!(
            r#"
name: poll-flow
stages:
  watch:
    kind: poll
    script_file: check.sh
    interval: 1s
    timeout: 30s
{GREEN_OR_RED}
    on: {{ green: finished, red: failed, timeout: stalled }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
  stalled:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        );
        let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
        let task_id = seed_task_in(&pool, &def.name, &dir).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let decisive = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(decisive["attempt"], json!(3));
        assert_eq!(decisive["outcome"], json!("green"));

        let events = poll_events(&pool, &task_id).await;
        assert_eq!(
            events.len(),
            2,
            "the repeated PENDING attempt should not have been recorded: {events:?}"
        );
        assert_eq!(events[0]["attempt"], json!(1));
        assert_eq!(events[0]["stdout_tail"], json!("PENDING"));
        assert!(
            events[0].get("note").is_none(),
            "a progress entry carries no note: {:?}",
            events[0]
        );
    }

    #[tokio::test]
    async fn a_poll_that_never_matches_gives_up_through_the_timeout_edge() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def("echo PENDING", &format!("    timeout: 1s\n{GREEN_OR_RED}"));
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "stalled").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["timed_out"], json!(true));
        assert!(
            event["note"].as_str().unwrap().contains("timeout elapsed"),
            "the timeout entry should say why: {event:?}"
        );
    }

    /// Unlike `shell`, a poll's exit code decides nothing — a `gh` that
    /// exits nonzero on a rate limit while still printing the state is the
    /// case polling exists to ride out. The output is what matters.
    #[tokio::test]
    async fn a_polls_exit_code_does_not_decide_its_outcome() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def("echo SUCCESS; exit 7", GREEN_OR_RED);
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "finished").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["exit_code"], json!(7));
        assert_eq!(event["outcome"], json!("green"));
    }

    /// …but a command that never starts is permanent, and retrying it on an
    /// interval would only burn the whole budget to reach the same place.
    #[tokio::test]
    async fn a_poll_command_that_cannot_start_takes_the_error_edge() {
        use std::os::unix::fs::PermissionsExt;

        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let script = dir.join("not-executable.sh");
        std::fs::write(&script, "#!/bin/sh\necho SUCCESS\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();

        let yaml = format!(
            r#"
name: poll-flow
stages:
  watch:
    kind: poll
    script_file: not-executable.sh
    interval: 1s
{GREEN_OR_RED}
    on: {{ green: finished, red: failed, error: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        );
        let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
        let task_id = seed_task_in(&pool, &def.name, &dir).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "failed").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["attempt"], json!(1));
        assert!(
            event["note"]
                .as_str()
                .unwrap()
                .contains("failed to start command"),
            "the error entry should say the command never ran: {event:?}"
        );
    }

    /// A poll with no `error` edge is waiting for a human on purpose, so it
    /// parks rather than wedging — and must not keep polling a command that
    /// can never start.
    #[tokio::test]
    async fn a_poll_with_no_error_edge_parks_instead_of_transitioning() {
        use std::os::unix::fs::PermissionsExt;

        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let script = dir.join("not-executable.sh");
        std::fs::write(&script, "#!/bin/sh\necho SUCCESS\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();

        let yaml = format!(
            r#"
name: poll-flow
stages:
  watch:
    kind: poll
    script_file: not-executable.sh
    interval: 1s
{GREEN_OR_RED}
    on: {{ green: finished, red: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        );
        let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
        let task_id = seed_task_in(&pool, &def.name, &dir).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_decisive_poll_event(&pool, &task_id).await;

        // Long enough that a loop which kept going would have run several
        // more attempts and recorded them.
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        assert_eq!(
            workflow_state::get(&pool, &task_id)
                .await
                .unwrap()
                .unwrap()
                .current_stage,
            "watch"
        );
        assert_eq!(poll_events(&pool, &task_id).await.len(), 1);
    }

    #[tokio::test]
    async fn a_poll_captures_the_matching_attempts_stdout() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def(
            r#"printf '{\"state\": \"SUCCESS\"}'"#,
            &format!("    capture: json\n{GREEN_OR_RED}"),
        );
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["watch"]["state"],
            json!("SUCCESS")
        );
    }

    /// A poll holds its stage open for as long as its budget allows, so
    /// unlike `shell` it really can be overtaken by a human. It must notice
    /// and stop rather than keep burning a command every interval — and
    /// must not drag the task back out of wherever it went.
    #[tokio::test]
    async fn a_poll_abandons_its_loop_once_the_task_leaves_the_stage() {
        let pool = connect_in_memory().await.unwrap();
        // Never matches and has no timeout, so nothing but the
        // stage-departure check can end this loop.
        let def = parsed_poll_def("echo PENDING", GREEN_OR_RED);
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        // Let the first attempt land, so the loop is genuinely running.
        wait_until_poll_attempt_recorded(&pool, &task_id).await;

        engine.advance(&task_id, &def, "red").await.unwrap();
        wait_until_stage(&pool, &task_id, "failed").await;
        let recorded = poll_events(&pool, &task_id).await.len();

        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        assert_eq!(
            workflow_state::get(&pool, &task_id)
                .await
                .unwrap()
                .unwrap()
                .current_stage,
            "failed",
            "the abandoned poll must not have advanced the task again"
        );
        assert_eq!(
            poll_events(&pool, &task_id).await.len(),
            recorded,
            "the poll should have stopped running its command"
        );
    }

    /// A poll's outcome goes through the stage's `on:` map like any other,
    /// so `loop_guard` applies to it without the kind knowing anything
    /// about guards.
    #[tokio::test]
    async fn a_polls_outcome_is_subject_to_loop_guards() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = format!(
            r#"
name: poll-flow
stages:
  watch:
    kind: poll
    command: "echo FAILURE"
    interval: 1s
{GREEN_OR_RED}
    on: {{ green: finished, red: watch }}
    loop_guard: {{ on: red, max: 1, then: stalled }}
  finished:
    kind: terminal
  stalled:
    kind: human_gate
    on: {{ resumed: finished }}
"#
        );
        let def = Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        // First `red` loops back into `watch`; the second exceeds the
        // guard and reroutes.
        wait_until_stage(&pool, &task_id, "stalled").await;
    }

    struct TempDir(PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "chokofactoryd-engine-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}
