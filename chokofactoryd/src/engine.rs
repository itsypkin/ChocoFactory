//! Workflow engine (design §5): the generic stage/transition interpreter
//! that drives a task's `workflow_state` through a loaded
//! `WorkflowDefinition`. The graph's topology comes entirely from the
//! definition (§5.1); this module only supplies the fixed, small
//! vocabulary of stage *behaviors* (§5.2). Phase 1 implements
//! `agent_turn`, `human_gate`, `terminal` — `shell`/`poll` are already
//! parsed by the loader (P1-6) but their execution lands in Phase 2
//! (P2-1/P2-2), so entering one here is a deliberate, reported error
//! rather than a silent no-op.
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

use chokofactory_core::models::{Task, TaskRunEndReason, TaskRunStatus};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::db::{projects, task_runs, tasks, workflow_state};
use crate::global_config::{GlobalConfig, GlobalConfigError};
use crate::role_config::{self, RoleConfigError};
use crate::session::{SessionError, SessionManager};
use crate::workflow_def::{StageDef, StageKind, WorkflowDefError, WorkflowDefinition};

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
    /// write, silently clobbering one call's `stage_history`/`loop_counters`.
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
}

#[derive(Debug)]
pub enum EngineError {
    NoWorkflowState,
    NoSuchTask,
    UnknownStage(String),
    UnknownOutcome { stage: String, outcome: String },
    TerminalStageHasNoTransitions(String),
    MissingAgentTurnInput(String),
    UnknownRole { stage: String, role: String },
    UnsupportedStageKind(String),
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
            EngineError::UnsupportedStageKind(stage) => write!(
                f,
                "stage '{stage}' has a kind the engine cannot execute yet"
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
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            session_manager,
            task_locks: Mutex::new(HashMap::new()),
            workflows_dir,
            global_config_path,
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
            self.enter_stage(task_id, definition, start, initial_input)
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

                let mut stage_history = match state.stage_history {
                    Value::Array(entries) => entries,
                    _ => Vec::new(),
                };
                stage_history.push(json!(from_stage));

                workflow_state::update(
                    &self.pool,
                    task_id,
                    workflow_state::WorkflowStateUpdate {
                        current_stage: next_stage.clone(),
                        loop_counters,
                        stage_history: Value::Array(stage_history),
                        payload: state.payload,
                    },
                )
                .await?;

                self.enter_stage(task_id, definition, &next_stage, None)
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

    /// Dispatches the behavior for whichever kind `stage_name` is (§5.2).
    /// `input` is only consulted for a `prompt_file`-less `agent_turn`.
    async fn enter_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        input: Option<&str>,
    ) -> Result<(), EngineError> {
        let stage_def = definition
            .stages
            .get(stage_name)
            .ok_or_else(|| EngineError::UnknownStage(stage_name.to_string()))?;

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
            StageKind::Shell { .. } | StageKind::Poll { .. } => {
                Err(EngineError::UnsupportedStageKind(stage_name.to_string()))
            }
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
        let session_manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(tokio::sync::Notify::new()),
        );
        // No test here drives create_task/send_message's workflow-name
        // resolution or a global config file, so an inert directory and
        // no config path are enough — role_config::resolve just falls
        // through to whatever the hand-built definition's `roles:` block
        // already specifies, exactly like before this field existed.
        WorkflowEngine::new(pool, session_manager, PathBuf::from("."), None)
    }

    fn engine_with_adapter_and_workflows_dir(
        pool: SqlitePool,
        binary: &str,
        workflows_dir: &Path,
    ) -> Arc<WorkflowEngine> {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let session_manager = SessionManager::new(
            pool.clone(),
            adapter,
            chrono::Duration::hours(1),
            Arc::new(tokio::sync::Notify::new()),
        );
        WorkflowEngine::new(pool, session_manager, workflows_dir.to_path_buf(), None)
    }

    async fn wait_until_stage(pool: &SqlitePool, task_id: &str, expected: &str) {
        for _ in 0..200 {
            let state = workflow_state::get(pool, task_id).await.unwrap().unwrap();
            if state.current_stage == expected {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for stage {expected}");
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
        assert_eq!(state.stage_history, json!([]));
    }

    #[tokio::test]
    async fn advance_transitions_through_the_on_map_and_records_history() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine.advance(&task_id, &def, "resumed").await.unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
        assert_eq!(state.stage_history, json!(["gate"]));
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
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "closed");
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

    #[tokio::test]
    async fn entering_an_unsupported_stage_kind_is_a_reported_error_not_a_panic() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: has-shell
stages:
  run:
    kind: shell
    command: "true"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        let err = engine.start_task(&task_id, &def, None).await.unwrap_err();
        assert!(matches!(err, EngineError::UnsupportedStageKind(stage) if stage == "run"));
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
