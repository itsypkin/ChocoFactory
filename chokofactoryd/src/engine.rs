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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chokofactory_core::models::{EventType, Task, TaskRunEndReason, TaskRunStatus};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::db::{events, projects, task_runs, tasks, workflow_state};
use crate::global_config::{GlobalConfig, GlobalConfigError};
use crate::poll;
use crate::role_config::{self, RoleConfigError};
use crate::session::{SessionError, SessionManager};
use crate::shell;
use crate::template;
use crate::workflow_def::{
    Capture, PollOutcome, ShellCommand, StageDef, StageKind, WorkflowDefError, WorkflowDefinition,
};
use crate::worktree::{self, WorktreeError};

/// How often the `agent_turn` completion watcher polls a `task_run`'s
/// status. Not configurable (yet) — this is an internal implementation
/// detail of auto-advancing single-shot turns, not a user-facing knob.
const TURN_WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// What a completed `agent_turn` transitions on when its reply carried no
/// verdict of its own — §5.2's "a plain single-shot turn just emits `done`".
const TURN_DEFAULT_OUTCOME: &str = "done";

/// In-flight detached `shell`/`poll` runners, keyed by task id and then by
/// the runner id [`WorkflowEngine::reserve_runner_slot`] hands out (#69).
///
/// The inner `Option` is the reservation: `None` between the slot being
/// claimed and its `JoinHandle` being attached, which is a window only the
/// spawning caller can observe.
type DetachedRunners = HashMap<String, HashMap<u64, Option<JoinHandle<()>>>>;

/// `tasks.status` for a task that reached a `terminal` stage (§5.4).
const TASK_STATUS_CLOSED: &str = "closed";

/// `tasks.status` for a task an operator cancelled (#69) — a third value
/// beyond §5.4's `open`/`closed`.
///
/// Distinct from `closed` on purpose: `closed` means the workflow reached
/// an end it declared, and a task can only get there by traversing its
/// graph. `cancelled` means a human stopped it somewhere it wasn't
/// designed to stop, which is the thing an operator scanning
/// `choco task list` most needs to be able to tell apart. It is also what
/// every guard in this file keys off to refuse further work on the task,
/// so collapsing the two would make "did this finish or was it killed?"
/// unanswerable from the API.
const TASK_STATUS_CANCELLED: &str = "cancelled";

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
    /// In-flight detached `shell`/`poll` runners per task, so `cancel_task`
    /// can stop them (#69).
    ///
    /// Neither stage kind opens a `task_run`, so killing the task's agent
    /// session reaches neither — yet both can be running a command for
    /// minutes, in the task's worktree, which cancel is about to delete.
    /// Aborting the runner drops its future mid-await, which drops
    /// `shell::run`'s `ProcessGroup` guard, which SIGKILLs the command's
    /// whole process group.
    ///
    /// A `std::sync::Mutex`, not tokio's: every critical section here is a
    /// map insert or remove with no `await` inside, and `spawn_shell_runner`
    /// /`spawn_poll_runner` are deliberately *synchronous* fns (they'd
    /// otherwise reintroduce an auto-trait inference cycle — see their doc
    /// comments), so they cannot await a lock at all.
    ///
    /// Entry lifecycle is the part worth getting right, since a map keyed
    /// by task id is exactly the shape this codebase's reviews keep finding
    /// leaks in: a slot is reserved *before* the spawn and removed by the
    /// runner itself when it finishes, and the whole task entry is dropped
    /// once its last runner is gone, so nothing accumulates for tasks that
    /// are never cancelled.
    detached_runners: std::sync::Mutex<DetachedRunners>,
    /// Source of the ids keying `detached_runners`' inner maps. Only needs
    /// to be unique per task, but a single global counter is simpler than
    /// per-task numbering and just as correct.
    next_runner_id: AtomicU64,
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
    /// An operator cancelled the task while a detached runner was still in
    /// flight (#69), so its outcome was discarded rather than advancing a
    /// task that is meant to have stopped. Expected, not a fault — a cancel
    /// races whatever was already running by definition.
    TaskCancelled(String),
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
    /// A `{{ stages.… }}`/`{{ task.… }}` reference in this stage's
    /// `command:`/`prompt_file` with genuinely malformed syntax (P2-3,
    /// §5.1) — an unterminated placeholder, whitespace in the path, an
    /// unrecognized root. A *missing value* — a field the referenced
    /// stage's capture didn't actually carry, or a stage that hasn't
    /// captured anything yet — is not this: `template::render`/
    /// `render_command` substitute an empty string for that instead and
    /// report it via `record_unresolved_template_note` (#60), since there's
    /// no way for the loader to have caught it ahead of time. Malformed
    /// syntax specifically *is* caught at load time
    /// (`WorkflowDefinition::validate`), so reaching this variant at all
    /// means a hand-built definition bypassed that check — `roles`/`stages`
    /// are `pub` fields with no private-construction guard (§ review on PR
    /// #35) — this stays a reported error rather than an `.expect()`
    /// defensively, not because it's an expected run-time path.
    Template {
        stage: String,
        reason: String,
    },
    Session(SessionError),
    Db(sqlx::Error),
    Io(std::io::Error),
    GlobalConfig(GlobalConfigError),
    RoleConfig(RoleConfigError),
    /// Resolving or creating a worktree-enabled workflow's working
    /// directory failed (§5.5 Q7, issue #58) — see `WorkingDirError`.
    Worktree(WorkingDirError),
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
            EngineError::TaskCancelled(task_id) => {
                write!(f, "task '{task_id}' was cancelled and cannot be advanced")
            }
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
            EngineError::Template { stage, reason } => {
                write!(f, "stage '{stage}' could not render a template: {reason}")
            }
            EngineError::Session(err) => write!(f, "{err}"),
            EngineError::Db(err) => write!(f, "{err}"),
            EngineError::Io(err) => write!(f, "{err}"),
            EngineError::GlobalConfig(err) => write!(f, "{err}"),
            EngineError::RoleConfig(err) => write!(f, "{err}"),
            EngineError::Worktree(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<sqlx::Error> for EngineError {
    fn from(err: sqlx::Error) -> Self {
        EngineError::Db(err)
    }
}

impl From<WorkingDirError> for EngineError {
    fn from(err: WorkingDirError) -> Self {
        EngineError::Worktree(err)
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
    /// The task was cancelled (#69). Detected under the per-task lock, so
    /// unlike `send_message_or_resume`'s own earlier check this one cannot
    /// be raced by a concurrent `cancel_task`.
    TaskCancelled,
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    RoleConfig(RoleConfigError),
    GlobalConfig(GlobalConfigError),
    Session(SessionError),
    Db(sqlx::Error),
    /// Same as `EngineError::Worktree` — relaying a message into a
    /// worktree-enabled workflow's open stage needs the same working
    /// directory the stage itself runs in.
    Worktree(WorkingDirError),
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
            SendMessageError::TaskCancelled => {
                write!(f, "task was cancelled and accepts no further messages")
            }
            SendMessageError::Resolve(err) => write!(f, "{err}"),
            SendMessageError::WorkflowDef(err) => write!(f, "{err}"),
            SendMessageError::RoleConfig(err) => write!(f, "{err}"),
            SendMessageError::GlobalConfig(err) => write!(f, "{err}"),
            SendMessageError::Session(err) => write!(f, "{err}"),
            SendMessageError::Db(err) => write!(f, "{err}"),
            SendMessageError::Worktree(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SendMessageError {}

impl From<sqlx::Error> for SendMessageError {
    fn from(err: sqlx::Error) -> Self {
        SendMessageError::Db(err)
    }
}

impl From<WorkingDirError> for SendMessageError {
    fn from(err: WorkingDirError) -> Self {
        SendMessageError::Worktree(err)
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
    /// The task was cancelled (#69), so it accepts no further messages or
    /// resume signals regardless of what stage it stopped in.
    TaskCancelled,
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
            SendMessageOrResumeError::TaskCancelled => {
                write!(f, "task was cancelled and accepts no further messages")
            }
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

#[derive(Debug)]
pub enum CancelTaskError {
    NoSuchTask,
    /// Already cancelled, or already `closed` by reaching a terminal stage.
    /// Both are 409s rather than silent no-ops: a second cancel is either a
    /// duplicate request the caller should know about, or an attempt to
    /// cancel work that already finished on its own — and answering `202`
    /// to the latter would imply the daemon stopped something it didn't.
    NotCancellable(String),
    NoWorkflowState,
    /// A session for this task's run is mid-spawn, so the process to kill
    /// doesn't exist yet and isn't reachable from here. The caller can
    /// retry once that settles.
    Session(SessionError),
    Db(sqlx::Error),
}

impl fmt::Display for CancelTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CancelTaskError::NoSuchTask => write!(f, "no such task"),
            CancelTaskError::NotCancellable(status) => {
                write!(f, "task is already '{status}' and cannot be cancelled")
            }
            CancelTaskError::NoWorkflowState => write!(f, "task has no workflow_state row"),
            CancelTaskError::Session(err) => write!(f, "{err}"),
            CancelTaskError::Db(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CancelTaskError {}

impl From<sqlx::Error> for CancelTaskError {
    fn from(err: sqlx::Error) -> Self {
        CancelTaskError::Db(err)
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
            detached_runners: std::sync::Mutex::new(HashMap::new()),
            next_runner_id: AtomicU64::new(0),
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
        // Takes the same per-task lock `start_task`/`advance` use, so that
        // *every* path which can establish an agent session for a task
        // holds it (#69). This one is the odd one out historically: the
        // others reach `SessionManager` through `enter_agent_turn` inside
        // the lock, while this resumes a session directly without it.
        //
        // That mattered once `cancel_task` existed. Cancel holds this lock
        // and then asks `SessionManager` to kill the task's run; if a
        // resume could be mid-spawn at that moment, cancel would see
        // `Establishing`, fail *after* having already marked the task
        // cancelled, and leave a live agent attached to a task whose
        // status now makes every retry a 409 — an agent nothing could ever
        // kill. Holding the lock here makes that interleaving impossible
        // rather than merely unlikely.
        //
        // Not re-entrant with `send_message_or_resume`: that function
        // doesn't hold the lock when it delegates here (its `human_gate`
        // branch takes the other path, into `advance_from_stage`), and
        // nothing in this function calls `advance`. `tokio::sync::Mutex`
        // is not reentrant, so that separation is load-bearing — see this
        // module's header.
        let lock = self.lock_for_task(task_id).await;
        let result = {
            let _guard = lock.lock().await;
            self.send_message_locked(task_id, text).await
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    /// The body of [`Self::send_message`], run under that function's
    /// per-task lock. Split out so the guard's scope is a single
    /// statement rather than the whole function.
    async fn send_message_locked(
        self: &Arc<Self>,
        task_id: &str,
        text: &str,
    ) -> Result<(), SendMessageError> {
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageError::NoSuchTask)?;

        // Re-checked here, under the lock, and not only in
        // `send_message_or_resume` (#69). That caller reads `status`
        // before taking any lock and then does real work — resolving a
        // workflow path, loading and parsing its YAML — so a cancel can
        // easily land in between. The window is not theoretical: for a
        // chat task whose turn has finished, the run is `idle`, so
        // `cancel_task` finds nothing to kill and returns having only
        // written the status. Without this check the send would then
        // resume a *fresh* subprocess from the persisted `session_id` —
        // spawning an agent for a task the operator already cancelled,
        // and one that no retry could kill, since every later cancel is a
        // 409. The `human_gate` branch is safe only because
        // `advance_from_stage` re-checks under this same lock; this is
        // the matching check for the branch that resumes a session.
        if task.status == TASK_STATUS_CANCELLED {
            return Err(SendMessageError::TaskCancelled);
        }

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
        let cwd = working_dir(&task, &definition)?;
        let resolved = role_config::resolve(
            role,
            role_def,
            &global,
            &task.config,
            cwd,
            definition.worktree,
        )
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
    /// whichever of `send_message`/`advance_from_stage` it actually needs
    /// (P1-9): a standing-open `agent_turn` relays `text` straight into its
    /// live session via `send_message`; a `human_gate` has no session to
    /// relay into at all — the human's `text` is the resume signal itself,
    /// so this transitions it directly (§59), threading `text` through as
    /// the gate's capture (if it declared `capture: text`) the same way a
    /// `shell`/`poll` stage's output is threaded through today. Any other
    /// stage kind (a mid-transition `agent_turn`, `shell`, `poll`,
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

        // A cancelled task accepts nothing further (#69). Without this the
        // dispatch below would still match on *stage kind* — which cancel
        // deliberately leaves untouched — and a task parked in a
        // standing-open `agent_turn` would take the message, find no live
        // session (cancel killed it), and resume a fresh subprocess from
        // the persisted `session_id`: restarting the very process the
        // operator just stopped. `tasks.status` is the only thing that
        // distinguishes that task from a healthy one here.
        if task.status == TASK_STATUS_CANCELLED {
            return Err(SendMessageOrResumeError::TaskCancelled);
        }

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
            StageKind::HumanGate { capture } => {
                let (captured, note) = derive_capture(
                    *capture,
                    text,
                    task_id,
                    &current_stage,
                    "the human's message",
                );

                // Best-effort, log-and-continue — same as `send_message`'s
                // chat-path recording just above it in this file. No
                // `task_run` exists for a `human_gate` (it never opens a
                // session), so this uses `append_for_task` — the same
                // task-scoped, session-less path `dispatch_stage` already
                // uses for `StageEntered`/`Error` — rather than `append`,
                // which needs a `task_run_id` to derive `task_id` from.
                let mut payload = json!({ "text": text });
                if let Some(note) = note {
                    payload["note"] = json!(note);
                }
                match events::append_for_task(&self.pool, task_id, EventType::HumanMessage, payload)
                    .await
                {
                    Ok(_) => self.events_notify.notify_waiters(),
                    Err(err) => {
                        tracing::error!(task_id, %err, "failed to record human message event")
                    }
                }

                self.advance_from_stage(
                    task_id,
                    &definition,
                    "resumed",
                    Some(&current_stage),
                    captured,
                )
                .await
                .map_err(SendMessageOrResumeError::Advance)
            }
            StageKind::AgentTurn { .. } if stage_def.on.is_empty() => self
                .send_message(task_id, text)
                .await
                .map_err(SendMessageOrResumeError::SendMessage),
            _ => Err(SendMessageOrResumeError::UnsupportedStageKind(
                current_stage,
            )),
        }
    }

    /// Stops `task_id` for good at an operator's request (#69): marks it
    /// `cancelled`, kills whatever subprocess group it has running, and
    /// removes its worktree.
    ///
    /// The ordering below is the whole design, so it is worth stating why.
    /// It is arranged around one rule — **nothing that can fail happens
    /// after the status write** — which is what lets both of the properties
    /// this function needs hold at once:
    ///
    /// 1. The two fallible *reads* (`workflow_state`, the current
    ///    `task_run`) come first, so a DB error here returns having changed
    ///    nothing at all and a retry starts clean.
    /// 2. `tasks.status` is written **second**, still inside the per-task
    ///    lock. Every guard that makes cancel stick — `advance_from_stage`,
    ///    `send_message`, `run_poll_stage` — reads that column, so it has
    ///    to land before this function releases the lock. Taking the same
    ///    lock `advance` takes means an in-flight transition either
    ///    completes entirely before this write or observes it; it cannot
    ///    interleave. A crash immediately after leaves a task that is
    ///    genuinely cancelled and will never run another stage.
    /// 3. Everything that actually stops work — the session kill, the
    ///    detached-runner abort, the worktree removal — comes **last**,
    ///    because none of it can fail in a way that should abort the
    ///    cancel. Doing any of it before the write would reintroduce the
    ///    window this ordering exists to close: a killed agent attached to
    ///    a task the engine still believes is `open`, which a later
    ///    `task send` would happily resume from the persisted
    ///    `session_id`.
    /// 4. Within that last group the worktree goes after the kills. `git
    ///    worktree remove --force` against a directory an agent is still
    ///    writing to is a race, and killing first shrinks it to nothing.
    ///
    /// An earlier revision of this function killed first and wrote second,
    /// on the reasoning that a failed kill should not leave a task marked
    /// cancelled. That is the wrong trade: a cancel that half-succeeded and
    /// left the status unwritten is indistinguishable from a healthy task,
    /// whereas a cancelled task whose kill failed is at least visibly
    /// stopped. Making the reads fallible-first gets the good half of both.
    ///
    /// Cancelling does not walk the workflow to a terminal stage.
    /// `current_stage` deliberately stays where it was, so
    /// `choco task status` can still say *where* a task was cancelled;
    /// `tasks.status` alone carries the "don't run this any more" signal.
    pub async fn cancel_task(self: &Arc<Self>, task_id: &str) -> Result<(), CancelTaskError> {
        let lock = self.lock_for_task(task_id).await;
        let result = {
            let _guard = lock.lock().await;

            // Read inside the lock, not before it: two concurrent cancels
            // would otherwise both see `open`, both pass the check, and
            // both proceed to kill and remove the worktree.
            let task = tasks::get(&self.pool, task_id)
                .await?
                .ok_or(CancelTaskError::NoSuchTask)?;
            if task.status == TASK_STATUS_CANCELLED || task.status == TASK_STATUS_CLOSED {
                Err(CancelTaskError::NotCancellable(task.status))
            } else {
                self.cancel_task_locked(&task).await
            }
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    /// The body of [`Self::cancel_task`], split out only so the per-task
    /// lock guard's scope stays obvious at the call site above.
    async fn cancel_task_locked(self: &Arc<Self>, task: &Task) -> Result<(), CancelTaskError> {
        let task_id = &task.id;

        // Step 1 — the fallible reads, before anything is written or
        // killed, so a DB error here returns having changed nothing.
        //
        // A missing `workflow_state` row is *not* an error: a task whose
        // `start_task` died between `worktree::ensure` and
        // `workflow_state::create` never reached a stage, so it has no
        // session to kill — which is precisely the state cancel wants. It
        // may still own a worktree, and treating this as an error would
        // mark the task cancelled, skip the removal below, and then refuse
        // every retry with a 409, leaking that worktree permanently.
        let state = workflow_state::get(&self.pool, task_id).await?;
        let run = match &state {
            Some(state) => {
                task_runs::get_current_for_stage(&self.pool, task_id, &state.current_stage).await?
            }
            None => None,
        };

        // Step 2 — the write every guard keys off, and the last thing here
        // that can fail. After this the task is durably cancelled: no stage
        // will advance, no message will be accepted, and a crash on the
        // very next line leaves a task that is visibly stopped rather than
        // one that silently looks healthy.
        tasks::update_status(&self.pool, task_id, TASK_STATUS_CANCELLED)
            .await?
            // `None` means the row vanished between this function's own
            // read and this write — impossible while the lock is held, but
            // surfaced rather than discarded, the same way `start_task`
            // treats it.
            .ok_or(CancelTaskError::NoSuchTask)?;
        tracing::info!(task_id, "task cancelled");

        // Step 3 — stop the work. Nothing below is allowed to fail the
        // cancel: the task is already cancelled, and returning an error now
        // would strand it behind a permanent 409 with no way to retry the
        // very cleanup that failed.
        if let Some(run) = run
            && run.status == TaskRunStatus::Active
        {
            // The only error `SessionManager::cancel` can return is
            // `AlreadyStarting` — a session mid-spawn, which this call can
            // neither see nor kill. It is unreachable from here, and
            // deliberately so: every path that establishes a session for a
            // task (`start_task`, `advance` → `enter_agent_turn`, and
            // `send_message`) holds the same per-task lock this function
            // holds, so no spawn for this task can be in flight right now.
            //
            // `send_message` only started taking that lock as part of this
            // change, and this is why. Without it the interleaving was: a
            // resume reserves the slot, cancel marks the task `cancelled`
            // and then fails here — leaving a live agent attached to a task
            // whose status makes every retry a 409, so nothing could ever
            // kill it.
            //
            // Logged at `error` rather than propagated, because propagating
            // would recreate exactly that: the status write above has
            // already happened, so an `Err` here strands the task instead
            // of letting an operator retry. If the lock invariant is ever
            // broken this is the line that says so.
            if let Err(err) = self.session_manager.cancel(&run.id).await {
                tracing::error!(
                    task_id, task_run_id = %run.id, %err,
                    "cancelled task's session could not be killed; a live agent may have been left running"
                );
            }
        }

        // An `agent_turn` is not the only thing that can be running. A
        // `shell` or `poll` stage runs detached, owns no `task_run` row,
        // and — for a `worktree: true` workflow — has the worktree as its
        // cwd. Aborting the runner drops the future mid-await, which drops
        // `shell::run`'s `ProcessGroup` guard, which SIGKILLs the
        // command's whole group. Without this, cancel would leave a `make
        // && npm test` running and then delete the directory out from
        // under it.
        self.abort_detached_runners(task_id).await;

        // The worktree last, once nothing is writing to it: both the agent
        // session and any detached `shell`/`poll` command have been killed
        // above, so `git worktree remove --force` isn't racing a live
        // writer.
        //
        // Gated on the snapshot rather than on `definition.worktree` so
        // this needs no workflow definition at all — and so a
        // worktree-enabled task cancelled before it ever reached a stage
        // that called `worktree::ensure` doesn't trip `remove_worktree`'s
        // "no snapshot to remove" error log for a worktree that was never
        // created.
        if worktree_snapshot(task).is_some() {
            self.remove_worktree(task_id).await;
        }
        Ok(())
    }

    /// Claims an id for a detached `shell`/`poll` runner about to be
    /// spawned for `task_id` (#69).
    ///
    /// Reserved *before* `tokio::spawn`, not after, and that ordering is
    /// the point: if the slot were only created once the handle existed, a
    /// runner that finished in between would call [`Self::finish_runner`]
    /// for an id not yet present, and the later insert would then leak an
    /// entry for a task that has nothing running.
    ///
    /// Callers must hold the task's `task_locks` entry across the
    /// reserve/spawn/attach sequence — every one does today, since the only
    /// callers are reached from `enter_stage`, which runs inside
    /// `advance`/`start_task`'s guard. That is load-bearing rather than
    /// incidental: `cancel_task` holds the same lock, so it cannot observe
    /// a half-built slot. A runner spawned outside the lock could have its
    /// reservation aborted (and dropped) between the reserve and the
    /// attach, at which point the attach silently no-ops and the runner is
    /// left with nothing able to stop it.
    fn reserve_runner_slot(&self, task_id: &str) -> u64 {
        let id = self.next_runner_id.fetch_add(1, Ordering::Relaxed);
        self.detached_runners
            .lock()
            .expect("detached_runners mutex poisoned")
            .entry(task_id.to_string())
            .or_default()
            .insert(id, None);
        id
    }

    /// Fills in the handle for a slot [`Self::reserve_runner_slot`]
    /// claimed. A slot that has already been removed means the runner
    /// finished first, so there is deliberately nothing to do — inserting
    /// it back would be the leak the reservation exists to avoid.
    fn attach_runner_handle(&self, task_id: &str, id: u64, handle: JoinHandle<()>) {
        let mut runners = self
            .detached_runners
            .lock()
            .expect("detached_runners mutex poisoned");
        if let Some(slot) = runners.get_mut(task_id).and_then(|task| task.get_mut(&id)) {
            *slot = Some(handle);
        }
    }

    /// Drops a finished runner's slot, and the task's whole entry once its
    /// last runner is gone, so the map doesn't grow without bound for
    /// tasks nobody ever cancels.
    fn finish_runner(&self, task_id: &str, id: u64) {
        let mut runners = self
            .detached_runners
            .lock()
            .expect("detached_runners mutex poisoned");
        if let Some(task) = runners.get_mut(task_id) {
            task.remove(&id);
            if task.is_empty() {
                runners.remove(task_id);
            }
        }
    }

    /// Aborts every detached `shell`/`poll` runner in flight for `task_id`
    /// (#69), killing the command each one is running.
    ///
    /// Abort drops the runner's future at its current await point, which
    /// drops `shell::run`'s `ProcessGroup` guard, whose `Drop` SIGKILLs the
    /// command's whole process group — the same teardown a `timeout:`
    /// already relies on. A runner that has already finished aborts
    /// harmlessly.
    ///
    /// The entry is removed wholesale: an aborted runner never reaches its
    /// own `finish_runner` call, so nothing else would clean it up.
    async fn abort_detached_runners(&self, task_id: &str) {
        let handles: Vec<JoinHandle<()>> = {
            let mut runners = self
                .detached_runners
                .lock()
                .expect("detached_runners mutex poisoned");
            runners
                .remove(task_id)
                .map(|task| task.into_values().flatten().collect())
                .unwrap_or_default()
        };
        if handles.is_empty() {
            return;
        }
        tracing::info!(
            task_id,
            runners = handles.len(),
            "cancelling task: aborting its in-flight shell/poll runners"
        );
        // `abort` only *schedules* the task to be dropped, and the SIGKILL
        // happens in that drop — so awaiting each handle afterwards is what
        // makes this deterministic rather than hopeful. A cancelled
        // `JoinHandle` resolves once the future has actually been dropped,
        // which is precisely the point the command's process group has been
        // killed. Without the await, the caller could go on to
        // `git worktree remove --force` the directory those commands are
        // still running in.
        for handle in handles {
            handle.abort();
            // The expected outcome is `Err(JoinError::Cancelled)`. `Ok` is
            // a runner that finished on its own just before the abort, and
            // a panicked runner is already reported by its own task — so
            // neither is worth handling here, only waiting for.
            let _ = handle.await;
        }
    }

    /// Whether `task_id` has been cancelled (#69).
    ///
    /// Advisory helper for long-running detached work — see
    /// `run_poll_stage`. A `true` here is authoritative (the column is only
    /// ever set one way), but a `false` can go stale the moment it's read,
    /// so this must never be the *only* thing standing between a cancelled
    /// task and a transition. `advance_from_stage`'s check, taken inside
    /// the per-task lock, is what actually enforces it.
    ///
    /// A failed read answers `false` — "keep going" — matching
    /// `still_in_stage`'s handling of the same case: a transient DB error
    /// should not silently abandon a task's in-flight work.
    async fn is_cancelled(&self, task_id: &str) -> bool {
        match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task.status == TASK_STATUS_CANCELLED,
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, %err,
                    "could not check whether a task was cancelled; assuming it was not"
                );
                false
            }
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
    /// created with (§5.4). It's always seeded into `payload.task.input`
    /// (P2-7a), reachable from any stage's `prompt_file`/`command` as
    /// `{{ task.input }}` — but only used *directly*, as the turn's own
    /// prompt, when the entry stage is an `agent_turn` with no
    /// `prompt_file`.
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
            // The task's title/initial input is seeded under `payload.task`
            // (P2-7a, §5.1) — a sibling of `merge_stage_capture`'s `stages`
            // key, so it can never collide with a workflow's own stage
            // names — reachable from any stage's `prompt_file` as
            // `{{ task.input }}`/`{{ task.title }}`. It's the only thing in
            // the entry stage's payload: no stage has run yet, so any
            // `{{ stages.… }}` in it is unresolvable by construction.
            // Looked up fresh here (rather than threaded in as a parameter)
            // because `start_task` is also called directly, without going
            // through `create_task`, wherever a task's own row already
            // carries the title this needs.
            let task = tasks::get(&self.pool, task_id)
                .await?
                .ok_or(EngineError::NoSuchTask)?;
            // The same guard `advance_from_stage` has, for the same reason
            // and under the same lock (#69). `create_task` writes the task
            // row and only then calls this, so a cancel can land in that
            // window; without this check, starting would go on to create a
            // worktree and spawn an agent for a task already marked
            // `cancelled` — one that every later cancel refuses with a 409.
            if task.status == TASK_STATUS_CANCELLED {
                return Err(EngineError::TaskCancelled(task_id.to_string()));
            }
            // Forked once, before `workflow_state` exists at all, so a
            // failure here never leaves a task with a `workflow_state` row
            // pointing at an entry stage whose worktree was never created
            // (§5.5 Q7, issue #58). The `(repo, project)` pair `ensure` used
            // is then snapshotted onto the task row — every later lookup
            // (`working_dir`, terminal-stage removal) reads that snapshot
            // rather than re-resolving `config.cwd`/the project's name,
            // which can both change out from under a running task (see
            // `worktree_creation_inputs`'s doc comment).
            if definition.worktree {
                let (repo, project) = worktree_creation_inputs(&self.pool, &task).await?;
                worktree::ensure(&repo, &project, &task.id)
                    .await
                    .map_err(WorkingDirError::Worktree)?;
                // `None` means the task row was deleted out from under this
                // call (same race `tasks::get` above is exposed to) —
                // surfaced the same way, not silently ignored.
                tasks::set_worktree(&self.pool, &task.id, &repo.to_string_lossy(), &project)
                    .await?
                    .ok_or(EngineError::NoSuchTask)?;
            }
            let payload = json!({ "task": { "input": initial_input, "title": task.title } });
            let state = workflow_state::create(&self.pool, task_id, start, payload).await?;
            self.enter_stage(
                task_id,
                definition,
                start,
                initial_input,
                None,
                &state.payload,
            )
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
    /// The `expected_stage`/`capture`-less form of `advance_from_stage`, for
    /// a caller with nothing to guard or thread through. Every in-process
    /// caller that has either — the `shell`/`poll`/`agent_turn` completion
    /// watchers spawned by `enter_stage`, and `send_message_or_resume`'s
    /// `human_gate` relay (#59) — calls `advance_from_stage` directly
    /// instead, so this simpler form is currently exercised only by tests;
    /// kept `pub` as the natural entry point for a caller that genuinely
    /// has no stage to guard against and nothing to capture.
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
                // The authoritative cancel guard (#69). Every detached
                // runner in this file — the turn watcher, the shell runner,
                // the poll runner — funnels its outcome through here, so
                // one check inside the per-task lock stops all of them
                // rather than each having to remember to look.
                //
                // Placed inside the lock for the same reason
                // `expected_stage` is, and the reason it can't just be read
                // in `cancel_task` and cached: `cancel_task` takes this same
                // lock and writes `tasks.status` under it, so a read here
                // either sees that write or is ordered entirely before it.
                // Outside the lock, a turn finishing at the same instant as
                // a cancel could read `open`, then advance a task the
                // operator had already stopped.
                if let Some(task) = tasks::get(&self.pool, task_id).await?
                    && task.status == TASK_STATUS_CANCELLED
                {
                    return Err(EngineError::TaskCancelled(task_id.to_string()));
                }

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

                // The returned row is the authority on what was actually
                // committed, and it's what the next stage renders its
                // templates against (P2-3). A `None` here means the row
                // vanished between this function's read and its write —
                // impossible while the lock is held, but discarding the
                // `Option` would turn that broken invariant into a task that
                // silently transitions against state nothing persisted.
                let updated = workflow_state::update(
                    &self.pool,
                    task_id,
                    workflow_state::WorkflowStateUpdate {
                        current_stage: next_stage.clone(),
                        loop_counters,
                        payload,
                    },
                )
                .await?
                .ok_or(EngineError::NoWorkflowState)?;

                // `enter_stage` records the transition itself (X-3), so the
                // trail this used to push onto `workflow_state.stage_history`
                // now lives in the events timeline with a timestamp and the
                // outcome that caused it.
                self.enter_stage(
                    task_id,
                    definition,
                    &next_stage,
                    None,
                    Some(outcome),
                    &updated.payload,
                )
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
    ///
    /// `payload` is the task's `workflow_state.payload` as the caller just
    /// committed it, and is what this stage's `{{ stages.… }}` references
    /// render against (P2-3, §5.1). It's passed in rather than re-read
    /// because both callers hold the per-task lock and already have the
    /// authoritative value: re-reading here would be a second query for the
    /// same row, and — worse — would invite a future caller to render
    /// against state some other writer had moved on from.
    async fn enter_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        input: Option<&str>,
        entered_via: Option<&str>,
        payload: &Value,
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

        let entered =
            self.dispatch_stage(task_id, definition, stage_name, stage_def, input, payload);
        let entered = entered.await;

        // A missing *value* no longer reaches here at all (#60) — it's
        // substituted as an empty string and reported via
        // `record_unresolved_template_note` instead, from wherever
        // `template::render`/`render_command` actually ran. What's left is
        // genuinely malformed syntax, which the loader already catches for
        // anything built through it — see `EngineError::Template`'s own
        // doc comment for why this still isn't dead code. On a detached
        // path this would otherwise leave the timeline showing a stage
        // entered and then nothing at all, with the reason only in the
        // daemon's log, so it's still recorded the same way. Task-scoped,
        // since rendering happens before any `task_run` exists.
        if let Err(EngineError::Template { stage, reason }) = &entered {
            let message = format!("stage '{stage}' could not render a template: {reason}");
            tracing::error!(task_id, stage, reason, "stage parked: {message}");
            match events::append_for_task(
                &self.pool,
                task_id,
                EventType::Error,
                json!({ "stage": stage, "message": message }),
            )
            .await
            {
                Ok(_) => self.events_notify.notify_waiters(),
                Err(err) => tracing::error!(
                    task_id, stage, %err,
                    "failed to record a template failure event"
                ),
            }
        }
        entered
    }

    /// The per-kind behavior half of `enter_stage`, split out so the caller
    /// can act on the result once rather than at five `return` sites.
    async fn dispatch_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        stage_def: &StageDef,
        input: Option<&str>,
        payload: &Value,
    ) -> Result<(), EngineError> {
        match &stage_def.kind {
            StageKind::AgentTurn {
                role,
                prompt_file,
                capture,
            } => {
                self.enter_agent_turn(
                    task_id,
                    definition,
                    stage_name,
                    stage_def,
                    role,
                    prompt_file.as_deref(),
                    *capture,
                    input,
                    payload,
                )
                .await
            }
            // Pauses the task with nothing further to do here; whatever
            // relays the next human message is responsible for advancing
            // this stage on `"resumed"` once it arrives — see
            // `send_message_or_resume`'s `HumanGate` arm, which also
            // threads the message through as this stage's capture (#59)
            // if it declared one.
            StageKind::HumanGate { .. } => Ok(()),
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
                if let Err(err) =
                    tasks::update_status(&self.pool, task_id, TASK_STATUS_CLOSED).await
                {
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
                // Same best-effort reasoning as `update_status` above (§5.5
                // Q7, issue #58): the task is already closed regardless of
                // whether this succeeds, and `worktree::remove` is itself
                // idempotent — `cancel_task` (#69) can safely retry this
                // exact removal if it raced this one.
                if definition.worktree {
                    self.remove_worktree(task_id).await;
                }
                Ok(())
            }
            StageKind::Shell {
                command,
                capture,
                timeout,
            } => {
                let (command, unresolved) = render_command(command, payload, stage_name)?;
                self.record_unresolved_template_note(task_id, stage_name, &unresolved)
                    .await;
                self.enter_shell(task_id, definition, stage_name, command, *capture, *timeout)
                    .await
            }
            StageKind::Poll {
                command,
                capture,
                interval,
                timeout,
                outcomes,
            } => {
                let (command, unresolved) = render_command(command, payload, stage_name)?;
                self.record_unresolved_template_note(task_id, stage_name, &unresolved)
                    .await;
                self.enter_poll(
                    task_id, definition, stage_name, command, *capture, *interval, *timeout,
                    outcomes,
                )
                .await
            }
        }
    }

    /// Removes `task_id`'s worktree, best-effort — logged loudly on
    /// failure, never propagated (§5.5 Q7, issue #58).
    ///
    /// Called from `dispatch_stage`'s `StageKind::Terminal` arm and from
    /// `cancel_task` (#69) — §5.5's "removed on reaching `done` (or task
    /// cancellation)". Both call it only *after* the task is already
    /// durably `closed`/`cancelled`, so there is nothing left here that a
    /// returned error could still roll back. `worktree::remove` is
    /// idempotent, so the two paths racing each other is safe.
    ///
    /// Callers are responsible for having checked that a worktree should
    /// exist at all — the terminal arm via `definition.worktree`,
    /// `cancel_task` via `worktree_snapshot` — since this logs an error
    /// when a task it is asked to clean up carries no snapshot.
    async fn remove_worktree(self: &Arc<Self>, task_id: &str) {
        let task = match tasks::get(&self.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                tracing::error!(task_id, "task disappeared before worktree removal");
                return;
            }
            Err(err) => {
                tracing::error!(task_id, %err, "failed to load task for worktree removal");
                return;
            }
        };
        let Some((repo, project)) = worktree_snapshot(&task) else {
            tracing::error!(
                task_id,
                "worktree-enabled task has no worktree_repo/worktree_project snapshot to remove"
            );
            return;
        };
        match worktree::remove(&repo, project, &task.id).await {
            Ok(()) => tracing::info!(task_id, "worktree removed (task closed)"),
            Err(err) => tracing::error!(
                task_id, %err,
                "failed to remove worktree after entering terminal stage"
            ),
        }
    }

    /// Records every placeholder a stage's template fell back to an empty
    /// string for (#60), best-effort — logged loudly on failure, never
    /// propagated, same pattern as every other event append in this file.
    /// No-op when nothing was unresolved, so a caller can call this
    /// unconditionally after every render. Task-scoped (`append_for_task`,
    /// no `task_run_id`): a template renders before any turn/session
    /// exists, whether it's an `agent_turn`'s prompt or a `shell`/`poll`
    /// stage's `command:`.
    async fn record_unresolved_template_note(
        &self,
        task_id: &str,
        stage_name: &str,
        placeholders: &[String],
    ) {
        if placeholders.is_empty() {
            return;
        }
        tracing::warn!(
            task_id,
            stage = stage_name,
            ?placeholders,
            "stage template referenced a value that isn't there yet; rendered as empty"
        );
        match events::append_for_task(
            &self.pool,
            task_id,
            EventType::TemplateUnresolved,
            json!({ "stage": stage_name, "placeholders": placeholders }),
        )
        .await
        {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, %err,
                "failed to record a template-unresolved event"
            ),
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
        let cwd = working_dir(&task, definition)?;

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
        // Registered so `cancel_task` can abort this runner and kill the
        // command it's running (#69) — a `shell` stage has no `task_run`,
        // so killing the task's agent session would not reach it.
        let runner_id = self.reserve_runner_slot(&task_id);
        let registered_task_id = task_id.clone();
        let handle = tokio::spawn(async move {
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
            engine.finish_runner(&task_id, runner_id);
        });
        self.attach_runner_handle(&registered_task_id, runner_id, handle);
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
            derive_capture(capture, &outcome.stdout, task_id, stage_name, "stdout")
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
            // Expected, not a wedge: an operator cancelled mid-command and
            // the guard in `advance_from_stage` refused the transition, so
            // the task is stopped on purpose and nothing is waiting on it.
            // Matched ahead of the catch-all below so a routine cancel
            // isn't reported at `error` as a task needing rescue.
            Err(EngineError::TaskCancelled(_)) => tracing::info!(
                task_id,
                stage = stage_name,
                outcome,
                "discarded a shell stage's outcome: the task was cancelled"
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
        let cwd = working_dir(&task, definition)?;

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
                cwd,
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
        // Same registration as `spawn_shell_runner`, and more load-bearing
        // here: a `poll` holds its window open for minutes or hours, so
        // without this a cancelled task keeps firing its command every
        // interval until the deadline. The advisory `is_cancelled` check in
        // `run_poll_stage` only fires *between* attempts; this stops one
        // already in flight.
        let runner_id = self.reserve_runner_slot(&task_id);
        let registered_task_id = task_id.clone();
        let handle = tokio::spawn(async move {
            engine
                .run_poll_stage(&task_id, &definition, &stage_name, run)
                .await;
            engine.finish_runner(&task_id, runner_id);
        });
        self.attach_runner_handle(&registered_task_id, runner_id, handle);
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
        // The most recent attempt that actually ran, so every timeout path
        // can report what the command last said rather than an empty entry.
        let mut last_outcome: Option<shell::ShellOutcome> = None;

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

            // Cancel needs its own check here, and can't ride on
            // `still_in_stage` above: cancelling deliberately leaves
            // `current_stage` where it was, so a cancelled poll is still
            // "in its stage" and would keep firing its command every
            // interval — an hour of `gh pr checks` on a task the operator
            // already stopped. Advisory, exactly like the check above; the
            // authoritative refusal is in `advance_from_stage`.
            if attempt > 0 && self.is_cancelled(task_id).await {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    attempts = attempt,
                    "abandoned a poll: the task was cancelled"
                );
                return;
            }

            let remaining =
                deadline.map(|at| at.saturating_duration_since(std::time::Instant::now()));
            if remaining == Some(Duration::ZERO) {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
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
                    // This attempt produced no outcome, and an *older* one
                    // must not be reported under this attempt's number — a
                    // timeout entry saying "attempt 7" while carrying
                    // attempt 6's exit code and output would be a quietly
                    // wrong record. Clearing it means both timeout paths
                    // below report empty fields after an I/O failure, which
                    // is honest about what the last attempt actually
                    // yielded: nothing.
                    last_outcome = None;
                    if self
                        .sleep_before_next_attempt(run.interval, deadline)
                        .await
                        .is_break()
                    {
                        self.finish_poll_timed_out(
                            task_id,
                            definition,
                            stage_name,
                            &described,
                            attempt,
                            last_outcome.as_ref(),
                        )
                        .await;
                        return;
                    }
                    continue;
                }
            };

            if let Some(matched) = run.outcomes.matching(&outcome.stdout) {
                let (captured, capture_note) =
                    derive_capture(run.capture, &outcome.stdout, task_id, stage_name, "stdout");
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
                warn_if_escaped(task_id, stage_name, &outcome);
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
            warn_if_escaped(task_id, stage_name, &outcome);

            // A killed attempt is reported once, by `finish_poll_timed_out`
            // below, which carries the same fields plus the reason. Passing
            // it through here as well would put two entries on the timeline
            // for one attempt.
            if !killed {
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
            }

            // Kept so whichever timeout path fires can report what the
            // command last actually said. Without it, a poll that printed
            // `PENDING` for an hour and then ran out of budget leaves a
            // final timeline entry showing nothing at all — the repeated
            // attempts having been deliberately suppressed above.
            last_outcome = Some(outcome);

            if killed {
                self.finish_poll_timed_out(
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
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
                    task_id,
                    definition,
                    stage_name,
                    &described,
                    attempt,
                    last_outcome.as_ref(),
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
            // Squarely reachable: a poll's advisory cancel check only runs
            // between attempts, so an outcome resolving in the same window
            // as a cancel lands here.
            Err(EngineError::TaskCancelled(_)) => tracing::info!(
                task_id,
                stage = stage_name,
                outcome,
                "discarded a poll stage's outcome: the task was cancelled"
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
        capture: Option<Capture>,
        input: Option<&str>,
        payload: &Value,
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
            // A workflow-authored prompt is templated against earlier stages'
            // captures (P2-3, §5.1) — this is how a reviewer's verdict
            // reaches the coder's next turn. Live human input is not: it's
            // what a person typed, and quietly rewriting parts of it would be
            // both surprising and a way to smuggle payload contents into a
            // message the human believes they authored.
            Some(path) => {
                let raw = fs::read_to_string(path).map_err(EngineError::Io)?;
                let (rendered, unresolved) =
                    template::render(&raw, payload).map_err(|err| EngineError::Template {
                        stage: stage_name.to_string(),
                        reason: err.to_string(),
                    })?;
                self.record_unresolved_template_note(task_id, stage_name, &unresolved)
                    .await;
                rendered
            }
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
        let cwd = working_dir(&task, definition)?;
        let resolved = role_config::resolve(
            role,
            role_def,
            &global,
            &task.config,
            cwd,
            definition.worktree,
        )
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
        // This is also why the loader rejects `capture:` on such a stage:
        // with no watcher there is no moment at which it could be taken.
        if !stage_def.on.is_empty() {
            self.spawn_turn_watcher(
                task_id.to_string(),
                Arc::clone(definition),
                stage_name.to_string(),
                capture,
                task_run.id,
            );
        }
        Ok(())
    }

    /// Watches a single-shot `agent_turn`'s `task_run` for completion, takes
    /// its `capture:` if it declared one, and auto-advances.
    ///
    /// Without a `capture:` the outcome is `done`, which is what §5.2 says a
    /// plain single-shot turn emits. With `capture: json` it is instead read
    /// from the reply's reserved `outcome` key (#45) — one mechanism serving
    /// both the `on:` transition and the values later stages template in,
    /// rather than a separate verdict channel.
    ///
    /// A crashed/non-zero exit is logged and left for a human to notice
    /// rather than guessing an outcome the stage's `on:` map was never
    /// designed to receive.
    fn spawn_turn_watcher(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        capture: Option<Capture>,
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
                    // Ordered ahead of every status arm below, because it
                    // is the one reason that decides the outcome on its own
                    // (#69): a cancelled run lands on `Exited` normally, but
                    // a turn that finished cleanly in the instant before
                    // the kill landed lands on `Idle` — and the `Idle` arm
                    // below would `break` and advance a task the operator
                    // had already stopped. `advance_from_stage`'s guard
                    // would still refuse that transition, so this is the
                    // early, quiet exit rather than the thing that makes
                    // cancel correct.
                    Ok(Some(run)) if run.end_reason == Some(TaskRunEndReason::Cancelled) => {
                        tracing::info!(
                            task_id,
                            task_run_id,
                            "task run was cancelled; not auto-advancing"
                        );
                        return;
                    }
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
            engine
                .finish_turn(&task_id, &definition, &stage_name, capture, &task_run_id)
                .await;
        });
    }

    /// Applies a completed turn's capture and outcome. Runs detached, like
    /// `finish_shell_stage`/`finish_poll_stage`, so there is nothing to
    /// return a failure to — it is logged and the task parks.
    async fn finish_turn(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        capture: Option<Capture>,
        task_run_id: &str,
    ) {
        let (captured, outcome, note) = match capture {
            None => (None, TURN_DEFAULT_OUTCOME.to_string(), None),
            Some(capture) => {
                // The turn's text isn't held anywhere in memory: the adapter
                // stream is drained straight into `events` by `drain_session`
                // and dropped, and this watcher only ever sees `task_runs`
                // rows. So the reply is read back from the timeline.
                match events::final_assistant_text_for_run(&self.pool, task_run_id).await {
                    Ok(reply) => {
                        let reply = unwrap_code_fence(reply.trim());
                        let (captured, capture_note) =
                            derive_capture(Some(capture), reply, task_id, stage_name, "the reply");
                        let (outcome, outcome_note) = turn_outcome(capture, captured.as_ref());
                        (captured, outcome, capture_note.or(outcome_note))
                    }
                    // Nothing to capture and no basis for a verdict, so this
                    // does not fall through to a default outcome — a task
                    // that can't read its own turn back parks for a human.
                    Err(err) => {
                        tracing::error!(
                            task_id, task_run_id, stage = stage_name, %err,
                            "could not read a turn's reply back to capture it; not auto-advancing"
                        );
                        self.append_turn_outcome_event(
                            task_id,
                            task_run_id,
                            json!({
                                "stage": stage_name,
                                "capture": capture_label(Some(capture)),
                                "outcome": Value::Null,
                                "applied": false,
                                "note": format!("the turn's reply could not be read back: {err}"),
                            }),
                        )
                        .await;
                        return;
                    }
                }
            }
        };

        // `expected_stage` below catches a task that has *left* this stage,
        // but not one that left and came back: re-entering opens a new
        // `task_run`, and a late watcher for the superseded one would pass
        // that check and overwrite the fresh capture with a stale verdict.
        // Advisory only, like poll's `still_in_stage` — it runs outside the
        // lock, and nothing can produce that interleaving today (nothing
        // moves a task out of an `agent_turn` while its run is live), so this
        // is the invariant announcing itself rather than a known case.
        if !self
            .is_current_run_for_stage(task_id, stage_name, task_run_id)
            .await
        {
            tracing::warn!(
                task_id,
                task_run_id,
                stage = stage_name,
                "discarded a turn's outcome: its stage has since started a newer run"
            );
            return;
        }

        // `expected_stage` matters even though a turn holds its stage open:
        // a human can close or resume the task between the run going idle
        // and this write, and the capture is keyed by the stage the check
        // confirms is still current.
        let applied = self
            .advance_from_stage(task_id, definition, &outcome, Some(stage_name), captured)
            .await;

        let applied_note = match &applied {
            Ok(()) => {
                tracing::debug!(
                    task_id,
                    task_run_id,
                    stage = stage_name,
                    outcome,
                    "turn completed; advanced"
                );
                None
            }
            // Deliberately parked, not broken — the same classification
            // `finish_shell_stage` uses. A reviewer stage that declares only
            // `approved`/`changes_requested` and whose reply carried neither
            // lands here, which is the intended place for a human to pick it
            // up rather than the engine inventing a transition.
            Err(EngineError::UnknownOutcome { stage, outcome }) => {
                tracing::info!(
                    task_id,
                    stage,
                    outcome,
                    "turn parked: its outcome has no 'on:' edge"
                );
                Some(format!(
                    "parked: stage '{stage}' has no 'on:' edge for '{outcome}'"
                ))
            }
            Err(EngineError::StageMovedOn { expected, actual }) => {
                tracing::info!(
                    task_id,
                    expected,
                    actual,
                    outcome,
                    "discarded a turn's outcome: the task had already left that stage"
                );
                Some(format!(
                    "not applied: the task had already left '{expected}' for '{actual}'"
                ))
            }
            // A turn that completed in the same instant it was cancelled.
            // The note goes on the `turn_outcome` event, so the timeline
            // says why the verdict wasn't applied rather than leaving a
            // reader to infer it from the task's status.
            Err(EngineError::TaskCancelled(_)) => {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    outcome,
                    "discarded a turn's outcome: the task was cancelled"
                );
                Some("not applied: the task was cancelled".to_string())
            }
            Err(err) => {
                tracing::error!(
                    task_id, stage = stage_name, outcome, %err,
                    "task wedged: its turn completed but the transition failed"
                );
                Some(format!("not applied: {err}"))
            }
        };

        // Written *after* the advance, and carrying whether it was applied,
        // so the entry can't claim a transition that was rejected — the park
        // this feature's lenient fallback relies on is exactly the case where
        // the outcome is computed but deliberately not taken.
        //
        // The cost of that ordering: `advance_from_stage` records the next
        // stage's `stage_entered` first, so on the timeline this entry sits
        // just *after* the transition it explains (and after a fast next
        // stage's own output). `shell_output` is written before its advance
        // and so reads the other way round. Accepted deliberately: an entry
        // that is one line late is a smaller problem than one that asserts a
        // transition which never happened.
        if capture.is_some() {
            // Only added when the stage actually parked: an author whose
            // `capture: text` turn routed fine through `on: { done: … }`
            // doesn't need to be told about `capture: json`. Added *here*
            // rather than suppressed later, so it can't swallow a note that
            // was explaining something else — an oversized reply that wasn't
            // stored at all is the one that must always survive.
            let text_hint = (capture == Some(Capture::Text) && applied.is_err()).then(|| {
                "'capture: text' keeps the reply but carries no verdict; use 'capture: json' \
                 to route on an 'outcome' key"
                    .to_string()
            });
            let note = [note, applied_note, text_hint]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let note = (!note.is_empty()).then(|| note.join("; "));
            self.append_turn_outcome_event(
                task_id,
                task_run_id,
                json!({
                    "stage": stage_name,
                    "capture": capture_label(capture),
                    "outcome": outcome,
                    "applied": applied.is_ok(),
                    "note": note,
                }),
            )
            .await;
        }
    }

    /// Whether `task_run_id` is still the newest run of `stage_name`.
    ///
    /// The `Err` arm errs towards proceeding: this only narrows a window
    /// nothing can reach today, and refusing to advance because a *check*
    /// failed would strand a task whose turn genuinely completed.
    ///
    /// Two caveats, both unreachable today and both deliberate. A `false`
    /// answer returns without a timeline entry, unlike the template failure
    /// above — it means two runs of one stage overlapped, which no path
    /// produces. And `get_current_for_stage` tie-breaks on a random UUID, so
    /// two runs started within one timestamp tick could pick the wrong
    /// "current" one and discard a legitimate outcome; that needs the same
    /// impossible overlap to happen at all.
    async fn is_current_run_for_stage(
        &self,
        task_id: &str,
        stage_name: &str,
        task_run_id: &str,
    ) -> bool {
        match task_runs::get_current_for_stage(&self.pool, task_id, stage_name).await {
            Ok(Some(current)) => current.id == task_run_id,
            // The run this watcher is for exists, so no row at all means the
            // task was deleted underneath it.
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, task_run_id, stage = stage_name, %err,
                    "could not confirm a completed turn is its stage's current run; advancing anyway"
                );
                true
            }
        }
    }

    async fn append_turn_outcome_event(&self, task_id: &str, task_run_id: &str, payload: Value) {
        match events::append(&self.pool, task_run_id, EventType::TurnOutcome, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, task_run_id, %err,
                "failed to record turn outcome event"
            ),
        }
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

/// `task.config.cwd`, with **no** fallback (unlike `task_cwd`, which falls
/// back to the daemon's own current directory for workflows — e.g. chat —
/// where the working directory doesn't matter). A worktree-enabled workflow
/// (§5.5 Q7, issue #58) always needs an explicit repo; silently falling back
/// here would risk forking a worktree next to whatever directory the daemon
/// happens to be running in, which is exactly the kind of surprise this
/// wiring exists to prevent.
fn task_repo(task: &Task) -> Option<PathBuf> {
    task.config
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Resolving a worktree-enabled workflow's working directory needs a repo
/// path and a project name before `worktree::ensure`/`worktree::
/// worktree_path` can run. Both `EngineError` and `SendMessageError` wrap
/// this the same way they already wrap `RoleConfigError`/`GlobalConfigError`,
/// since both entry points (`start_task`, and `send_message`) hit the same
/// failure modes.
#[derive(Debug)]
pub enum WorkingDirError {
    Db(sqlx::Error),
    /// `task.project_id` doesn't reference an existing project. Shouldn't
    /// happen given the FK, but not assumed away — see `CreateTaskError::
    /// NoSuchProject` for the same reasoning at task-creation time.
    NoSuchProject(String),
    /// The workflow definition opted into `worktree: true` but the task has
    /// no `config.cwd` set — there is no repo to fork a worktree from.
    MissingCwd(String),
    /// The workflow definition opted into `worktree: true`, but this task
    /// has no `worktree_repo`/`worktree_project` snapshot on it yet —
    /// `start_task` always writes one via `tasks::set_worktree` before any
    /// stage runs, so reaching this means a stage dispatched before
    /// `start_task` finished, not a normal task lifecycle.
    MissingWorktreeSnapshot(String),
    Worktree(WorktreeError),
}

impl fmt::Display for WorkingDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkingDirError::Db(err) => write!(f, "{err}"),
            WorkingDirError::NoSuchProject(id) => write!(f, "no such project '{id}'"),
            WorkingDirError::MissingCwd(task_id) => write!(
                f,
                "task '{task_id}' uses a worktree-enabled workflow but has no repo cwd configured"
            ),
            WorkingDirError::MissingWorktreeSnapshot(task_id) => write!(
                f,
                "task '{task_id}' uses a worktree-enabled workflow but has no worktree_repo/worktree_project snapshot recorded yet"
            ),
            WorkingDirError::Worktree(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WorkingDirError {}

impl From<sqlx::Error> for WorkingDirError {
    fn from(err: sqlx::Error) -> Self {
        WorkingDirError::Db(err)
    }
}

/// The repo path and project name to fork a worktree-enabled task's
/// worktree from — read from `task.config.cwd` and the project's *current*
/// name. Only ever called from `start_task`, right before `worktree::
/// ensure`, whose result `start_task` then snapshots onto `task.
/// worktree_repo`/`task.worktree_project` via `tasks::set_worktree`. Every
/// later lookup must read that snapshot (`working_dir`, terminal-stage
/// removal) instead of calling this again — `config.cwd` and the project's
/// name can both change after the worktree already exists (`PATCH
/// /tasks/{id}/config`, `PATCH /projects/{id}`), and re-deriving from their
/// current values would let a later stage compute a path `ensure` never
/// actually created.
async fn worktree_creation_inputs(
    pool: &SqlitePool,
    task: &Task,
) -> Result<(PathBuf, String), WorkingDirError> {
    let repo = task_repo(task).ok_or_else(|| WorkingDirError::MissingCwd(task.id.clone()))?;
    let project = projects::get(pool, &task.project_id)
        .await?
        .ok_or_else(|| WorkingDirError::NoSuchProject(task.project_id.clone()))?;
    Ok((repo, project.name))
}

/// The `(repo, project)` `start_task` snapshotted onto this task when its
/// worktree was created — see `worktree_creation_inputs`'s doc comment for
/// why this, not a fresh lookup, is what every later stage must use.
fn worktree_snapshot(task: &Task) -> Option<(PathBuf, &str)> {
    let repo = task.worktree_repo.as_deref()?;
    let project = task.worktree_project.as_deref()?;
    Some((PathBuf::from(repo), project))
}

/// A task's working directory for a stage that needs one: the task's
/// dedicated worktree if `definition.worktree` opted in, else today's
/// `task_cwd` (the task's configured repo directly, or a sensible
/// fallback). No I/O — `worktree::worktree_path` is a pure computation
/// (no filesystem access beyond validating identifiers), and the
/// `(repo, project)` pair comes from `task`'s own already-fetched snapshot,
/// not a fresh lookup.
fn working_dir(task: &Task, definition: &WorkflowDefinition) -> Result<PathBuf, WorkingDirError> {
    if !definition.worktree {
        return Ok(task_cwd(task));
    }
    let (repo, project) = worktree_snapshot(task)
        .ok_or_else(|| WorkingDirError::MissingWorktreeSnapshot(task.id.clone()))?;
    worktree::worktree_path(&repo, project, &task.id).map_err(WorkingDirError::Worktree)
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

/// Logs a poll attempt whose process group outlived its kill.
///
/// `run_shell_stage` warns on the same condition, and an operator grepping
/// logs for escaped process groups should find both kinds — a poll is if
/// anything the likelier source, since it can kill an attempt on every
/// interval for as long as its budget lasts.
fn warn_if_escaped(task_id: &str, stage_name: &str, outcome: &shell::ShellOutcome) {
    if outcome.escaped {
        tracing::warn!(
            task_id,
            stage = stage_name,
            "a poll attempt was killed but its process group could not be confirmed dead"
        );
    }
}

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

/// Turns what a stage produced — a command's stdout, or an `agent_turn`'s
/// reply — into the value stored under `payload.stages.<stage>`, plus an
/// optional note for the timeline when something about that needed
/// explaining. `source` names that output in the note ("stdout", "the
/// reply"), since the same rules serve every kind that can capture.
///
/// A stage with no `capture:` stores nothing at all — only a stage that
/// asked for its output to be kept gets a payload entry.
///
/// Unparseable JSON under `capture: json` is deliberately *not* an error.
/// For `shell`/`poll`, §5.2 makes the exit code (or the `outcomes:` match)
/// the only thing that decides the outcome, so the output is kept as text
/// and the stage still reports what its exit code said. An `agent_turn`
/// follows the same rule rather than a stricter one of its own: capture is
/// one mechanism, and a turn whose reply carries no usable verdict falls
/// back to the outcome a plain turn emits (see `turn_outcome`). The note is
/// what tells a reader why the value isn't the object they expected.
fn derive_capture(
    capture: Option<Capture>,
    output: &str,
    task_id: &str,
    stage_name: &str,
    source: &str,
) -> (Option<Value>, Option<String>) {
    let Some(capture) = capture else {
        return (None, None);
    };

    let trimmed = output.trim();
    if trimmed.len() > MAX_CAPTURE_BYTES {
        tracing::warn!(
            task_id,
            stage = stage_name,
            bytes = trimmed.len(),
            "stage output too large to capture; not stored"
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
                    "stage output was not valid JSON; captured as text"
                );
                (
                    Some(Value::String(trimmed.to_string())),
                    Some(format!(
                        "{source} was not valid JSON ({err}); captured as text"
                    )),
                )
            }
        },
    }
}

/// The outcome a completed `agent_turn` transitions on, and a note when that
/// wasn't what the stage's `capture:` implied it would be.
///
/// Under `capture: json` the reply's reserved `outcome` key is the verdict
/// (#45) — the whole point of capturing a turn as JSON. Anything else falls
/// back to `done`, the outcome §5.2 gives a plain single-shot turn.
///
/// That fallback is deliberate but not silent: it is recorded as a note on
/// the `turn_outcome` timeline entry and logged. The practical effect is that
/// a stage whose `on:` map declares real verdicts and no `done` edge — the
/// normal shape for a reviewer — parks for a human instead of guessing,
/// because `advance_from_stage` finds no `done` transition. A stage that
/// *does* declare `done` will take it, which is the accepted cost of one
/// lenient rule shared with `shell`/`poll` rather than two.
fn turn_outcome(capture: Capture, captured: Option<&Value>) -> (String, Option<String>) {
    // `capture: text` keeps the reply but has no reserved key to read a
    // verdict out of. That isn't worth remarking on by itself — plenty of
    // stages capture text and route on `done` quite correctly — so the
    // explanation is added by `finish_turn`, and only when the stage
    // actually parked.
    if capture != Capture::Json {
        return (TURN_DEFAULT_OUTCOME.to_string(), None);
    }
    match captured.and_then(|value| value.get("outcome")) {
        Some(Value::String(outcome)) if !outcome.is_empty() => (outcome.clone(), None),
        Some(Value::String(_)) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply's 'outcome' was empty; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
        Some(other) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply's 'outcome' was {}, not a string; advancing with \
                 '{TURN_DEFAULT_OUTCOME}'",
                json_type_of(other)
            )),
        ),
        None => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply carried no 'outcome' key; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
    }
}

/// Strips a surrounding ```` ``` ```` fence from a turn's reply.
///
/// The one normalization applied to a reply before it is captured, and it
/// earns its place: wrapping structured output in a fenced block is the single
/// commonest thing a model does unbidden, and without this a `capture: json`
/// reviewer would fail to parse, fall back to `done`, and route the graph on a
/// verdict it never gave. Everything else is left exactly as the agent wrote
/// it — this is not a general "find the JSON somewhere in the prose" search,
/// which would be guessing.
///
/// Only an *entire* reply that is one fenced block is unwrapped; a fence in
/// the middle of prose is left alone, since that reply wasn't a document.
///
/// Applied for `capture: text` too, not just `json`. A text capture of a
/// reply the agent chose to fence almost certainly wants the contents rather
/// than the markup, and one rule for both beats a mode-dependent surprise.
fn unwrap_code_fence(reply: &str) -> &str {
    let Some(rest) = reply.strip_prefix("```") else {
        return reply;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return reply;
    };
    // Drop the info string (` ```json `), which is the rest of the opening
    // line. A fence with no newline at all isn't a block.
    let Some((_info, body)) = body.split_once('\n') else {
        return reply;
    };
    // A second fence inside means this was prose containing two blocks, not
    // one document.
    if body.contains("```") {
        return reply;
    }
    body.trim()
}

fn json_type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// How a stage's `capture:` reads on the timeline.
fn capture_label(capture: Option<Capture>) -> Value {
    match capture {
        Some(Capture::Json) => Value::String("json".to_string()),
        Some(Capture::Text) => Value::String("text".to_string()),
        None => Value::Null,
    }
}

/// Substitutes a stage's earlier-captured values into an inline `command:`
/// (P2-3, §5.1).
///
/// A `script_file` is passed through untouched: §5.1 scopes templating to
/// `command:` and `prompt_file`, and rewriting an executable's contents on
/// the way to running it would be a different and much larger promise.
///
/// Rendered once here, on stage entry, rather than per attempt — a `poll`
/// re-runs the same command on every interval, and the payload cannot change
/// while the stage is current (captures land only on a transition), so
/// re-rendering would do identical work and invite the two to disagree.
/// Renders `command`'s templates, plus every placeholder that fell back to
/// an empty string doing so (#60) — the caller is responsible for surfacing
/// those (see `record_unresolved_template_note`), since this free function
/// has no access to `self`/the pool to do it itself.
fn render_command(
    command: &ShellCommand,
    payload: &Value,
    stage_name: &str,
) -> Result<(ShellCommand, Vec<String>), EngineError> {
    match command {
        ShellCommand::Inline(line) => {
            let (rendered, unresolved) =
                template::render(line, payload).map_err(|err| EngineError::Template {
                    stage: stage_name.to_string(),
                    reason: err.to_string(),
                })?;
            Ok((ShellCommand::Inline(rendered), unresolved))
        }
        ShellCommand::ScriptFile(path) => Ok((ShellCommand::ScriptFile(path.clone()), Vec::new())),
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
        workflow_state::create(&pool, &task_id, "gate", json!({}))
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
        engine.spawn_turn_watcher(
            task_id.clone(),
            Arc::clone(&def),
            "chatting".to_string(),
            None,
            task_run.id.clone(),
        );

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

        // `task` is the entry stage's only payload (P2-7a) — the stage
        // itself writes nothing, since it declares no `capture:`.
        assert_eq!(
            payload_of(&pool, &task_id).await,
            json!({"task": {"input": null, "title": "T"}})
        );
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
        assert_eq!(
            state.payload,
            json!({"task": {"input": null, "title": "T"}})
        );
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
        let (captured, note) = derive_capture(Some(Capture::Text), &huge, "task", "run", "stdout");
        assert!(captured.is_none());
        assert!(note.unwrap().contains("exceeds"));

        // The limit itself is fine.
        let at_limit = "y".repeat(MAX_CAPTURE_BYTES);
        let (captured, note) =
            derive_capture(Some(Capture::Text), &at_limit, "task", "run", "stdout");
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
        workflow_state::create(&pool, &task_id, "chatting", json!({}))
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
        workflow_state::create(&pool, &task_id, "ghost-stage", json!({}))
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

    /// #59: a `human_gate` that declares `capture: text` keeps the human's
    /// reply, and records it on the timeline — the two things the bug
    /// report says today's `HumanGate` arm drops entirely.
    fn write_capturing_human_gate_workflow(workflows_dir: &Path) {
        std::fs::write(
            workflows_dir.join("gated-capture.yaml"),
            r#"
name: gated-capture
stages:
  gate:
    kind: human_gate
    capture: text
    on: { resumed: done }
  done:
    kind: terminal
"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn send_message_or_resume_threads_a_human_gates_reply_through_as_its_capture() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_capturing_human_gate_workflow(&workflows_dir);
        let def =
            Arc::new(WorkflowDefinition::load(&workflows_dir.join("gated-capture.yaml")).unwrap());
        let task_id = seed_task(&pool, "gated-capture").await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine
            .send_message_or_resume(&task_id, "go fix the off-by-one in the loop guard")
            .await
            .unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
        assert_eq!(
            state.payload["stages"]["gate"],
            json!("go fix the off-by-one in the loop guard")
        );

        // Recorded task-scoped (no task_run — a human_gate never opens a
        // session), unlike the chat path's session-scoped HumanMessage.
        let recorded = events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == EventType::HumanMessage)
            .expect("expected a HumanMessage event");
        assert_eq!(recorded.task_run_id, None);
        assert_eq!(
            recorded.payload["text"],
            json!("go fix the off-by-one in the loop guard")
        );
    }

    /// A `human_gate` with no `capture:` declared keeps behaving exactly as
    /// before #59 — no payload entry, just the transition.
    #[tokio::test]
    async fn send_message_or_resume_on_a_non_capturing_human_gate_stores_no_payload() {
        let pool = connect_in_memory().await.unwrap();
        let workflows_dir = tempdir();
        write_human_gate_chain_workflow(&workflows_dir);
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, "gated").await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);
        engine.start_task(&task_id, &def, None).await.unwrap();

        engine
            .send_message_or_resume(&task_id, "ignored for a non-capturing human_gate")
            .await
            .unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "done");
        assert!(state.payload.get("stages").is_none());
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

    /// Pins the per-attempt cap: an attempt gets whatever is *left of the
    /// stage's budget*, not `interval`.
    ///
    /// The command here takes 3s under a 1s interval. Capping attempts at
    /// `interval` — which is what the field name intuitively suggests, and a
    /// very natural-looking "hardening" of that line — would SIGKILL this on
    /// every single attempt, so the stage could only ever end in `stalled`.
    /// Every other poll test uses a command that returns in milliseconds and
    /// would stay green through exactly that regression.
    #[tokio::test]
    async fn an_attempt_slower_than_the_interval_is_not_killed() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def(
            "sleep 3; echo SUCCESS",
            &format!("    timeout: 30s\n{GREEN_OR_RED}"),
        );
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "finished").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(
            event["attempt"],
            json!(1),
            "the slow command should have been allowed to finish on its first attempt"
        );
        assert_eq!(event["timed_out"], json!(false));
    }

    /// The other half of the same rule: an attempt that outlives the budget
    /// *is* killed, and the timeout entry carries what that last attempt
    /// managed to do rather than an empty placeholder.
    #[tokio::test]
    async fn an_attempt_that_outlives_the_budget_is_killed_and_reported() {
        let pool = connect_in_memory().await.unwrap();
        let def = parsed_poll_def(
            "echo waiting; sleep 30",
            &format!("    timeout: 2s\n{GREEN_OR_RED}"),
        );
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        wait_until_stage(&pool, &task_id, "stalled").await;
        let event = wait_until_decisive_poll_event(&pool, &task_id).await;
        assert_eq!(event["timed_out"], json!(true));
        assert_eq!(event["attempt"], json!(1));
        // Killed rather than exited on its own, and what it printed before
        // dying survives onto the timeline.
        assert_eq!(event["exit_code"], Value::Null);
        assert_eq!(event["stdout_tail"], json!("waiting"));

        // One entry for the one attempt, not two.
        let events = poll_events(&pool, &task_id).await;
        assert_eq!(
            events.len(),
            1,
            "a killed attempt should be reported once, not twice: {events:?}"
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

    // ---- P2-3 cross-stage templating (#14) and agent_turn capture (#45) ----

    /// An adapter "binary" that replies with exactly `reply`.
    ///
    /// A generated wrapper rather than an env var on the test process:
    /// `std::env::set_var` is process-global, and these tests run in parallel
    /// in one process, so two of them would clobber each other's reply.
    fn reply_binary(dir: &Path, reply: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let reply_path = dir.join("reply.txt");
        fs::write(&reply_path, reply).unwrap();

        let wrapper = dir.join("fake-claude-reply");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nFAKE_CLAUDE_REPLY_FILE='{}' exec '{}' \"$@\"\n",
                reply_path.display(),
                fixture_binary("fake_claude_reply.py"),
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        wrapper.display().to_string()
    }

    /// The `shell_output` entry for one particular stage — the plain
    /// `wait_until_shell_event` returns whichever came first, which isn't
    /// enough once a flow has two shell stages.
    async fn wait_until_shell_event_for(pool: &SqlitePool, task_id: &str, stage: &str) -> Value {
        for _ in 0..500 {
            let found = events::list_for_task(pool, task_id)
                .await
                .unwrap()
                .into_iter()
                .find(|e| {
                    e.event_type == EventType::ShellOutput
                        && e.payload.get("stage").and_then(Value::as_str) == Some(stage)
                });
            if let Some(event) = found {
                return event.payload;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a shell_output event for stage '{stage}'");
    }

    async fn wait_until_turn_outcome_event(pool: &SqlitePool, task_id: &str) -> Value {
        for _ in 0..500 {
            let found = events::list_for_task(pool, task_id)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.event_type == EventType::TurnOutcome);
            if let Some(event) = found {
                return event.payload;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a turn_outcome event");
    }

    /// The end-to-end shape §5.1 exists for: one stage captures, a later
    /// stage's `command:` reads a field out of that capture.
    #[tokio::test]
    async fn a_captured_field_is_templated_into_a_later_shell_command() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: templated
stages:
  open_pr:
    kind: shell
    command: "printf '{\"number\": 42, \"url\": \"http://pr/42\"}'"
    capture: json
    on: { done: report }
  report:
    kind: shell
    command: "echo checking {{ stages.open_pr.number }} at {{ stages.open_pr.url }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let event = wait_until_shell_event_for(&pool, &task_id, "report").await;
        // The *rendered* command is what the timeline records — an operator
        // debugging this needs the value that actually ran, not the template.
        assert_eq!(
            event["command"], "echo checking 42 at http://pr/42",
            "got {event}"
        );
        assert_eq!(event["stdout_tail"], "checking 42 at http://pr/42");
    }

    /// The other half of §5.1: the same substitution into a `prompt_file`,
    /// which is how a reviewer's verdict reaches the coder's next turn.
    #[tokio::test]
    async fn a_captured_field_is_templated_into_a_later_agent_turn_prompt() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        fs::write(
            dir.join("coder-turn.md"),
            "fix pr {{ stages.open_pr.number }}",
        )
        .unwrap();
        let yaml = r#"
name: templated
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  open_pr:
    kind: shell
    command: "printf '{\"number\": 42}'"
    capture: json
    on: { done: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "coding").await;

        // The fixture echoes back whatever prompt it was handed, so the
        // rendered text showing up as the reply proves what was sent.
        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        let run = runs.iter().find(|r| r.stage == "coding").unwrap();
        wait_until_events_contain(&pool, &run.id, "echo:fix pr 42").await;
    }

    /// #60's own motivating case: the same coder prompt is entered both
    /// before and after a review exists, referencing a reviewer's feedback
    /// that hasn't been captured yet on this — the first — pass. Before
    /// #60 this killed the task with `workflow_state.current_stage`
    /// permanently stuck at `coding`; now the turn runs with the
    /// placeholder blanked, and a note on the timeline says which one.
    #[tokio::test]
    async fn an_unresolved_prompt_placeholder_renders_empty_and_the_turn_still_runs() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        fs::write(
            dir.join("coder-turn.md"),
            "address: {{ stages.internal_review.feedback }}",
        )
        .unwrap();
        let yaml = r#"
name: templated
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: {}
  internal_review:
    kind: agent_turn
    role: coder
    capture: text
    on: { done: coding }
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "coding").await;

        // The turn ran at all — with the missing feedback blanked, not a
        // stuck task and a dead subprocess.
        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        let run = runs.iter().find(|r| r.stage == "coding").unwrap();
        wait_until_events_contain(&pool, &run.id, "echo:address: ").await;

        let note = events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == EventType::TemplateUnresolved)
            .unwrap_or_else(|| panic!("expected a template_unresolved note on the timeline"));
        assert_eq!(note.payload["stage"], json!("coding"));
        assert_eq!(
            note.payload["placeholders"],
            json!(["{{ stages.internal_review.feedback }}"])
        );
        assert_eq!(note.task_run_id, None);
    }

    /// P2-7a: this is the gap the issue closes — a `prompt_file` entry
    /// stage previously had no way to reach the task's own title/initial
    /// input, only a later stage's capture.
    #[tokio::test]
    async fn task_input_and_title_are_templated_into_the_entry_stage_prompt() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        fs::write(
            dir.join("coder-turn.md"),
            "{{ task.title }}: {{ task.input }}",
        )
        .unwrap();
        let yaml = r#"
name: templated
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine
            .start_task(&task_id, &def, Some("fix the flaky test"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "coding").await;

        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        let run = runs.iter().find(|r| r.stage == "coding").unwrap();
        // `seed_task` gives the task the title "T" (§ its own definition).
        wait_until_events_contain(&pool, &run.id, "echo:T: fix the flaky test").await;
    }

    /// `payload.task` is seeded once by `start_task` and must survive
    /// `advance_from_stage`'s payload carry-forward — it isn't only the
    /// entry stage's prompt that can reach it.
    #[tokio::test]
    async fn task_input_still_resolves_in_a_second_stage_prompt() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        fs::write(dir.join("coder-turn.md"), "{{ task.input }}").unwrap();
        let yaml = r#"
name: templated
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  setup:
    kind: shell
    command: "true"
    on: { done: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: {}
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine
            .start_task(&task_id, &def, Some("fix the flaky test"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "coding").await;

        let runs = task_runs::list_for_task(&pool, &task_id).await.unwrap();
        let run = runs.iter().find(|r| r.stage == "coding").unwrap();
        wait_until_events_contain(&pool, &run.id, "echo:fix the flaky test").await;
    }

    /// #60: the loader can only check that the referenced stage exists and
    /// captures something; whether its captured JSON actually carries the
    /// field is a run-time question. An absent one used to kill the task —
    /// `workflow_state.current_stage` was already committed to `report`
    /// before its command failed to render, with nothing left to ever move
    /// it — so it now renders empty and the task proceeds instead, with the
    /// blanked placeholder noted on the timeline rather than silently lost.
    #[tokio::test]
    async fn an_unresolved_field_renders_empty_and_the_task_proceeds() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: templated
stages:
  open_pr:
    kind: shell
    command: "printf '{\"number\": 42}'"
    capture: json
    on: { done: report }
  report:
    kind: shell
    command: "echo {{ stages.open_pr.missing }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;
        wait_until_task_status(&pool, &task_id, "closed").await;

        // The unrenderable placeholder became an empty string rather than
        // stopping the command from running at all.
        let ran = wait_until_shell_event_for(&pool, &task_id, "report").await;
        assert_eq!(ran["command"], json!("echo "));

        // The reason has to be discoverable, same as before #60 — the
        // difference is it's a note the task survives, not the whole
        // explanation for why it died.
        let note = events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == EventType::TemplateUnresolved)
            .unwrap_or_else(|| panic!("expected a template_unresolved note on the timeline"));
        assert_eq!(note.payload["stage"], json!("report"));
        assert_eq!(
            note.payload["placeholders"],
            json!(["{{ stages.open_pr.missing }}"])
        );
        assert_eq!(
            note.task_run_id, None,
            "a template renders before any session exists, so it is task-scoped"
        );
    }

    /// #45's headline case: a reviewer's structured reply drives both the
    /// `on:` transition and the value a later stage templates in.
    #[tokio::test]
    async fn a_capturing_turn_routes_on_its_replys_outcome_and_feeds_a_later_stage() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: reviewed
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { approved: report, changes_requested: report }
  report:
    kind: shell
    command: "echo {{ stages.review.comments }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, r#"{"outcome": "approved", "comments": "ship-it"}"#);
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        // Captured under the stage that produced it...
        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["review"]["outcome"], "approved");
        assert_eq!(payload["stages"]["review"]["comments"], "ship-it");

        // ...routed through the `on:` edge that the reply named...
        let trail = stage_trail(&pool, &task_id).await;
        assert!(
            trail
                .iter()
                .any(|(stage, outcome)| stage == "report" && outcome == "approved"),
            "got {trail:?}"
        );

        // ...and templated into the next stage's command.
        let event = wait_until_shell_event_for(&pool, &task_id, "report").await;
        assert_eq!(event["command"], "echo ship-it");
    }

    /// The case a real agent hits constantly: it narrates, uses a tool, and
    /// only then answers. Capturing everything it said would put prose in
    /// front of the JSON, fail to parse, and fall back to `done` — routing the
    /// graph on a verdict the reviewer never gave.
    #[tokio::test]
    async fn a_verdict_after_tool_use_is_captured_without_the_narration() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "TOOL\n{\"outcome\": \"approved\", \"n\": 1}");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(
            payload["stages"]["review"]["n"], 1,
            "the narration must not reach the capture: {payload}"
        );

        let trail = stage_trail(&pool, &task_id).await;
        assert!(
            trail
                .iter()
                .any(|(stage, outcome)| stage == "finished" && outcome == "approved"),
            "expected the reply's own verdict to route, got {trail:?}"
        );
    }

    /// Wrapping structured output in a fence is the commonest thing a model
    /// does unbidden; without unwrapping it the verdict never parses.
    #[tokio::test]
    async fn a_fenced_json_reply_is_captured() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "```json\n{\"outcome\": \"approved\"}\n```");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["review"]["outcome"], "approved");
    }

    /// Several assistant text blocks are one reply; the capture has to see
    /// the whole thing or the JSON won't parse.
    #[tokio::test]
    async fn a_reply_split_across_text_blocks_is_captured_as_one_document() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "BLOCKS\n{\"outcome\": \"approved\",| \"n\": 1}");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["review"]["n"], 1);
    }

    /// Decision taken with #45: a turn's capture follows the same lenient
    /// rule `shell`/`poll` use rather than a stricter one of its own. The
    /// reply is kept as text, the outcome falls back to `done`, and the note
    /// on the timeline is what stops that being silent.
    #[tokio::test]
    async fn a_reply_that_is_not_json_is_captured_as_text_and_falls_back_to_done() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "sorry, I could not do it");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(
            payload["stages"]["review"], "sorry, I could not do it",
            "an unparseable reply is kept as text"
        );

        let event = wait_until_turn_outcome_event(&pool, &task_id).await;
        assert_eq!(event["outcome"], "done");
        assert!(
            event["note"]
                .as_str()
                .is_some_and(|note| note.contains("not valid JSON")),
            "got {event}"
        );
    }

    #[tokio::test]
    async fn a_reply_without_an_outcome_key_falls_back_to_done_with_a_note() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, r#"{"comments": "no verdict here"}"#);
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        // Still captured — the payload is useful even without a verdict.
        let payload = payload_of(&pool, &task_id).await;
        assert_eq!(payload["stages"]["review"]["comments"], "no verdict here");

        let event = wait_until_turn_outcome_event(&pool, &task_id).await;
        assert_eq!(event["outcome"], "done");
        assert!(
            event["note"]
                .as_str()
                .is_some_and(|note| note.contains("no 'outcome' key")),
            "got {event}"
        );
    }

    /// The safety net behind that fallback: a reviewer stage declares real
    /// verdicts and no `done` edge, so a reply with no usable outcome parks
    /// the task for a human instead of taking a happy path.
    #[tokio::test]
    async fn a_capturing_turn_whose_fallback_has_no_edge_parks_the_task() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: reviewed
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { approved: finished, changes_requested: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "not json at all");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();

        let event = wait_until_turn_outcome_event(&pool, &task_id).await;
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "review");
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_ne!(task.status, "closed");

        // The entry must not claim a transition that was rejected: the
        // outcome was computed, and deliberately not taken.
        assert_eq!(event["applied"], false, "got {event}");
        assert!(
            event["note"]
                .as_str()
                .is_some_and(|note| note.contains("parked")),
            "got {event}"
        );
    }

    /// Decision taken with #45: only a stage that asks for a capture gets
    /// one. Without this a long-running chat stage would rewrite its whole
    /// transcript into `workflow_state` on every turn.
    #[tokio::test]
    async fn an_agent_turn_without_capture_stores_nothing_and_still_emits_done() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        fs::write(dir.join("turn.md"), "do the thing").unwrap();
        let yaml = r#"
name: plain
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, r#"{"outcome": "approved"}"#);
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        // Even though the reply *was* a JSON verdict, a stage that declared
        // no `capture:` neither stores it nor routes on it — `task` is the
        // only payload key present (P2-7a).
        assert_eq!(
            payload_of(&pool, &task_id).await,
            json!({"task": {"input": null, "title": "T"}})
        );
        let trail = stage_trail(&pool, &task_id).await;
        assert!(
            trail
                .iter()
                .any(|(stage, outcome)| stage == "finished" && outcome == "done"),
            "got {trail:?}"
        );
        let turn_events = events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::TurnOutcome)
            .count();
        assert_eq!(turn_events, 0, "no capture, nothing to report");
    }

    fn capturing_turn_yaml() -> &'static str {
        r#"
name: reviewed
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { approved: finished, done: finished }
  finished:
    kind: terminal
"#
    }

    #[test]
    fn unwrap_code_fence_unwraps_a_whole_fenced_reply() {
        assert_eq!(
            unwrap_code_fence("```json\n{\"outcome\": \"approved\"}\n```"),
            "{\"outcome\": \"approved\"}"
        );
        assert_eq!(
            unwrap_code_fence("```\n{\"a\": 1}\n```"),
            "{\"a\": 1}",
            "an absent info string is still a fence"
        );
    }

    /// Anything that isn't *entirely* one fenced block is left exactly as the
    /// agent wrote it — this is a narrow normalization, not a search for JSON
    /// hidden somewhere in prose.
    #[test]
    fn unwrap_code_fence_leaves_everything_else_alone() {
        for reply in [
            "{\"outcome\": \"approved\"}",
            "here you go:\n```json\n{\"a\": 1}\n```",
            "```json\n{\"a\": 1}\n``` and also ```\n{\"b\": 2}\n```",
            "```no newline```",
            "plain text",
            "",
        ] {
            assert_eq!(unwrap_code_fence(reply), reply, "for {reply:?}");
        }
    }

    #[test]
    fn turn_outcome_reads_the_replys_outcome_key() {
        let captured = json!({"outcome": "changes_requested", "comments": "nope"});
        let (outcome, note) = turn_outcome(Capture::Json, Some(&captured));
        assert_eq!(outcome, "changes_requested");
        assert!(note.is_none());
    }

    #[test]
    fn turn_outcome_falls_back_for_a_non_string_or_missing_outcome() {
        for captured in [
            json!({"outcome": 7}),
            json!({"outcome": ""}),
            json!({"comments": "x"}),
            json!("plain text"),
        ] {
            let (outcome, note) = turn_outcome(Capture::Json, Some(&captured));
            assert_eq!(outcome, "done", "for {captured}");
            assert!(note.is_some(), "for {captured}");
        }
    }

    /// `capture: text` says "keep the reply", not "read a verdict out of
    /// it" — there's no reserved key in a plain string to read. No note here:
    /// capturing text and routing on `done` is a perfectly correct thing to
    /// do, so the explanation belongs to the case that actually parks
    /// (`finish_turn`), not to every text-capturing turn.
    #[test]
    fn turn_outcome_is_done_for_a_text_capture() {
        let captured = json!("approved");
        let (outcome, note) = turn_outcome(Capture::Text, Some(&captured));
        assert_eq!(outcome, "done");
        assert_eq!(note, None);
    }

    /// A `capture: text` stage that routes correctly gets no lecture...
    #[tokio::test]
    async fn a_text_capture_that_routes_is_not_second_guessed() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: noted
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "looks good to me");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "finished").await;

        assert_eq!(
            payload_of(&pool, &task_id).await["stages"]["review"],
            "looks good to me"
        );
        let event = wait_until_turn_outcome_event(&pool, &task_id).await;
        assert_eq!(event["applied"], true);
        assert_eq!(event["note"], Value::Null, "got {event}");
    }

    /// ...but one that parks because it expected a verdict is told why.
    #[tokio::test]
    async fn a_text_capture_that_parks_is_told_what_would_have_routed() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = r#"
name: noted
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: text
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let binary = reply_binary(&dir, "approved");
        let engine = engine_with_adapter(pool.clone(), &binary);

        engine
            .start_task(&task_id, &def, Some("review this"))
            .await
            .unwrap();

        let event = wait_until_turn_outcome_event(&pool, &task_id).await;
        assert_eq!(event["applied"], false);
        let note = event["note"].as_str().unwrap_or_default();
        assert!(note.contains("capture: json"), "got {event}");
        assert!(note.contains("parked"), "got {event}");
    }

    #[test]
    fn render_command_substitutes_an_inline_command() {
        let payload = json!({"stages": {"open_pr": {"number": 42}}});
        let command = ShellCommand::Inline("gh pr checks {{ stages.open_pr.number }}".to_string());
        let (rendered, unresolved) = render_command(&command, &payload, "checks").unwrap();
        assert_eq!(
            rendered,
            ShellCommand::Inline("gh pr checks 42".to_string())
        );
        assert!(unresolved.is_empty());
    }

    #[test]
    fn render_command_leaves_a_script_file_alone() {
        let payload = json!({});
        let command = ShellCommand::ScriptFile(PathBuf::from("/tmp/run.sh"));
        let (rendered, unresolved) = render_command(&command, &payload, "run").unwrap();
        assert_eq!(rendered, command);
        assert!(unresolved.is_empty());
    }

    /// #60: a missing *value* renders empty and is reported back rather
    /// than failing the whole command — `render_command`'s counterpart to
    /// `template::render`'s own coverage of the same split.
    #[test]
    fn render_command_substitutes_empty_for_an_unresolvable_reference() {
        let payload = json!({"stages": {"open_pr": {"number": 42}}});
        let command = ShellCommand::Inline("echo {{ stages.open_pr.missing }}".to_string());
        let (rendered, unresolved) = render_command(&command, &payload, "report").unwrap();
        assert_eq!(rendered, ShellCommand::Inline("echo ".to_string()));
        assert_eq!(unresolved, vec!["{{ stages.open_pr.missing }}"]);
    }

    /// Malformed *syntax* is unaffected by #60 — still a hard error, still
    /// classified via `EngineError::Template`.
    #[test]
    fn render_command_still_reports_malformed_syntax() {
        let payload = json!({});
        let command = ShellCommand::Inline("echo {{ stages.open_pr".to_string());
        let err = render_command(&command, &payload, "report").unwrap_err();
        assert!(
            matches!(&err, EngineError::Template { stage, .. } if stage == "report"),
            "got {err}"
        );
    }

    // ---- P2-6 multi-role config resolution (#17) ----

    /// Like [`engine_with_adapter_and_workflows_dir`] but with a real global
    /// config file wired in. Every other test engine passes `None` there, so
    /// this is the only place the global layer participates end-to-end rather
    /// than only in `role_config`'s own unit tests.
    fn engine_with_global_config(
        pool: SqlitePool,
        binary: &str,
        workflows_dir: &Path,
        global_config_path: &Path,
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
            Some(global_config_path.to_path_buf()),
            events_notify,
        )
    }

    /// A two-role workflow (`coder` -> `reviewer` -> terminal) whose prompt
    /// files live next to the definition, mirroring §5.1's `coding-task.yaml`
    /// roles block. Each role deliberately leaves a *different* field unset so
    /// the three layers all have something to contribute:
    ///
    /// - `coder`: no `cli` (falls to global), `model` set here.
    /// - `reviewer`: no `cli` and no `model` (both fall to global).
    fn write_two_role_workflow(workflows_dir: &Path) {
        let prompts = workflows_dir.join("prompts");
        fs::create_dir_all(&prompts).unwrap();
        fs::write(prompts.join("coder-system.md"), "you write code").unwrap();
        fs::write(prompts.join("reviewer-system.md"), "you review code").unwrap();
        // `internal_review` isn't the entry stage, so it has no human input to
        // fall back on and needs its own turn prompt (as §5.1's real
        // `coding-task.yaml` gives every stage). `coding` deliberately has
        // none, so it exercises the entry-stage initial-input path instead.
        fs::write(prompts.join("reviewer-turn.md"), "review it").unwrap();
        fs::write(
            workflows_dir.join("multi-role.yaml"),
            r#"
name: multi-role
roles:
  coder:
    model: coder-def-model
    system_prompt_file: prompts/coder-system.md
  reviewer:
    system_prompt_file: prompts/reviewer-system.md
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: internal_review }
  internal_review:
    kind: agent_turn
    role: reviewer
    prompt_file: prompts/reviewer-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#,
        )
        .unwrap();
    }

    /// A global config supplying `cli` for both roles and a `model` for
    /// `reviewer` only.
    fn write_global_config(dir: &Path) -> PathBuf {
        let path = dir.join("config.yaml");
        fs::write(
            &path,
            r#"
roles:
  coder:
    cli: coder-global-cli
  reviewer:
    cli: reviewer-global-cli
    model: reviewer-global-model
"#,
        )
        .unwrap();
        path
    }

    /// Returns the run for `stage`, waiting for it to appear.
    async fn wait_until_run_for_stage(
        pool: &SqlitePool,
        task_id: &str,
        stage: &str,
    ) -> chokofactory_core::models::TaskRun {
        for _ in 0..500 {
            let found = task_runs::list_for_task(pool, task_id)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.stage == stage);
            if let Some(run) = found {
                return run;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for a task_run for stage {stage}");
    }

    /// The headline confirmation for #17/P2-6: a workflow that actually
    /// declares two roles resolves each of them independently, through all
    /// three layers, on a single task.
    ///
    /// Every field is sourced from a *different* layer, and the two roles
    /// disagree on every one of them, so a resolver that leaked one role's
    /// config into the other — or that resolved once and reused the result for
    /// the whole task — fails here rather than passing by coincidence:
    ///
    /// | role     | cli            | model                          | system prompt      |
    /// |----------|----------------|--------------------------------|--------------------|
    /// | coder    | global         | task-level (beats workflow-def) | workflow-def file  |
    /// | reviewer | global (other) | global                         | workflow-def file  |
    #[tokio::test]
    async fn a_two_role_workflow_resolves_each_role_independently() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let workflows_dir = dir.join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        write_two_role_workflow(&workflows_dir);
        let global_config_path = write_global_config(&dir);

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_global_config(
            pool.clone(),
            &fixture_binary("fake_claude_echo_args.py"),
            &workflows_dir,
            &global_config_path,
        );

        // Overrides for *both* roles at once — the task-level layer #17 is
        // about being able to supply for more than one role.
        let task = engine
            .create_task(
                &project_id,
                None,
                "multi-role",
                "T",
                "go",
                json!({
                    "roles": {
                        "coder": { "model": "coder-task-model" },
                        "reviewer": { "system_prompt": "inline reviewer prompt" }
                    }
                }),
            )
            .await
            .unwrap();

        // Each agent_turn completes and auto-advances with "done", so the
        // task walks coder -> reviewer -> finished on its own.
        wait_until_stage(&pool, &task.id, "finished").await;

        let coder_run = wait_until_run_for_stage(&pool, &task.id, "coding").await;
        let reviewer_run = wait_until_run_for_stage(&pool, &task.id, "internal_review").await;

        assert_eq!(coder_run.role, "coder");
        assert_eq!(reviewer_run.role, "reviewer");

        // `cli` came from the global layer, and each role got its *own* entry.
        assert_eq!(coder_run.cli_adapter, "coder-global-cli");
        assert_eq!(reviewer_run.cli_adapter, "reviewer-global-cli");

        // `model`: coder's task-level override beat the workflow-def's
        // `coder-def-model`; reviewer, unmentioned at the task level and
        // silent in the workflow def, fell through to global.
        assert_eq!(coder_run.model, "coder-task-model");
        assert_eq!(reviewer_run.model, "reviewer-global-model");

        // System prompts, read back off each subprocess's own argv: coder from
        // the workflow-def file, reviewer from its task-level inline text.
        // `multi-role.yaml` doesn't opt into `worktree: true`, so neither
        // role's spawn is sandboxed (#67) — `permission_mode` stays unset.
        wait_until_events_contain(
            &pool,
            &coder_run.id,
            "model=coder-task-model|system_prompt=you write code|permission_mode=<unset>",
        )
        .await;
        wait_until_events_contain(
            &pool,
            &reviewer_run.id,
            "model=reviewer-global-model|system_prompt=inline reviewer prompt|permission_mode=<unset>",
        )
        .await;
    }

    /// `role_config::resolve` re-reads `task.config` on every stage entry and
    /// caches nothing, so a `PATCH /tasks/{id}` between turns changes the
    /// *next* role's config while leaving the already-started run alone. This
    /// is what `choco task reconfigure` relies on.
    #[tokio::test]
    async fn reconfiguring_between_turns_affects_only_the_later_role() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let workflows_dir = dir.join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        // `coding` is a human_gate here so the task parks before the reviewer
        // turn, giving the reconfigure a deterministic window instead of a
        // race against an auto-advancing agent_turn.
        fs::write(workflows_dir.join("reviewer-turn.md"), "review it").unwrap();
        fs::write(
            workflows_dir.join("gated-review.yaml"),
            r#"
name: gated-review
roles:
  reviewer:
    cli: claude
    model: reviewer-def-model
stages:
  coding:
    kind: human_gate
    on: { resumed: internal_review }
  internal_review:
    kind: agent_turn
    role: reviewer
    prompt_file: reviewer-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#,
        )
        .unwrap();
        let global_config_path = write_global_config(&dir);

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let engine = engine_with_global_config(
            pool.clone(),
            &fixture_binary("fake_claude_echo_args.py"),
            &workflows_dir,
            &global_config_path,
        );

        let task = engine
            .create_task(&project_id, None, "gated-review", "T", "go", json!({}))
            .await
            .unwrap();
        wait_until_stage(&pool, &task.id, "coding").await;

        // Reconfigure while parked, then let the reviewer turn start.
        tasks::merge_config(
            &pool,
            &task.id,
            json!({ "roles": { "reviewer": { "model": "reviewer-patched-model" } } }),
        )
        .await
        .unwrap()
        .unwrap();

        let definition =
            Arc::new(WorkflowDefinition::load(&workflows_dir.join("gated-review.yaml")).unwrap());
        engine
            .advance(&task.id, &definition, "resumed")
            .await
            .unwrap();

        let run = wait_until_run_for_stage(&pool, &task.id, "internal_review").await;
        assert_eq!(
            run.model, "reviewer-patched-model",
            "the patched task config should beat the workflow def's model"
        );
    }

    // ---- worktree wiring (P2-7b, issue #58) -----------------------------

    async fn git(repo: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// A real, minimal git repo — `worktree::ensure` shells out to real
    /// `git`, so there's no mocking this at the engine level (same
    /// constraint `worktree.rs`'s own tests are under).
    async fn init_git_repo(dir: &Path) {
        git(dir, &["init", "-q"]).await;
        git(dir, &["config", "user.email", "test@example.com"]).await;
        git(dir, &["config", "user.name", "Test"]).await;
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "."]).await;
        git(dir, &["commit", "-q", "-m", "init"]).await;
    }

    /// Waits for `path` to stop existing — the counterpart to
    /// `wait_until_task_status`'s note that terminal-stage side effects
    /// (here, `worktree::remove`) can still be in flight for a moment after
    /// `tasks.status` already reads `closed`.
    async fn wait_until_path_gone(path: &Path) {
        for _ in 0..500 {
            if !path.exists() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for {path:?} to be removed");
    }

    #[tokio::test]
    async fn a_worktree_enabled_task_runs_stages_in_its_worktree_and_leaves_the_repo_untouched() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;

        // `done` lands on a human_gate, not `terminal` — this test is only
        // about where the stage ran, so it deliberately never reaches the
        // stage that would remove the worktree (see the removal test below).
        let yaml = r#"
name: worktree-flow
worktree: true
stages:
  run:
    kind: shell
    command: "touch ran-in-worktree"
    on: { done: verified, error: failed }
  verified:
    kind: human_gate
    on: { resumed: finished }
  failed:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "verified").await;

        let worktree_dir = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
        assert!(
            worktree_dir.join("ran-in-worktree").exists(),
            "expected the shell stage to have run inside the worktree"
        );
        assert!(
            !repo.join("ran-in-worktree").exists(),
            "the user's actual checkout must not be touched"
        );
        // The original checkout still has only what `init_git_repo` put
        // there — no new commit, no stray files from the stage.
        assert!(repo.join("README.md").exists());
    }

    #[tokio::test]
    async fn a_worktree_enabled_task_removes_its_worktree_on_reaching_a_terminal_stage() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;

        let yaml = r#"
name: worktree-terminal-flow
worktree: true
stages:
  run:
    kind: shell
    command: "exit 0"
    on: { done: finished, error: failed }
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: { resumed: finished }
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();

        let worktree_dir = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
        assert!(
            worktree_dir.exists(),
            "start_task should have created the worktree before returning"
        );

        wait_until_stage(&pool, &task_id, "finished").await;
        wait_until_task_status(&pool, &task_id, "closed").await;
        wait_until_path_gone(&worktree_dir).await;
    }

    #[tokio::test]
    async fn a_task_without_worktree_opt_in_never_creates_one() {
        let pool = connect_in_memory().await.unwrap();
        // No `git init` here at all — a non-opted-in workflow (like
        // `chat.yaml`) never calls into `worktree::ensure`, so `cwd` doesn't
        // even need to be a real repo.
        let repo = tempdir();
        let def = human_gate_chain_def();
        assert!(!def.worktree, "human_gate_chain_def must not opt in");

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "gate").await;

        let sibling = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
        assert!(
            !sibling.exists(),
            "a chat-style task must never get a worktree"
        );
    }

    /// Regression test for a review finding on this PR: `task.config.cwd`
    /// (`PATCH /tasks/{id}/config`) and a project's name (`PATCH
    /// /projects/{id}`) can both change after a worktree-enabled task's
    /// worktree already exists. A later stage must keep using the worktree
    /// `start_task` actually created — recomputing the path from the
    /// task/project's *current* values would derive a path `worktree::
    /// ensure` never created (and, on removal, `worktree::remove` would
    /// silently no-op against a path that was never real, leaking the
    /// original worktree on disk while logging success).
    #[tokio::test]
    async fn a_worktree_enabled_task_keeps_using_its_original_worktree_after_config_or_project_changes()
     {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;

        // `second` lands on a human_gate, not `terminal`, same reason as
        // the isolation test above: entering `done` would race this test's
        // own assertion against terminal-stage worktree removal (the
        // engine writes `current_stage = "done"` — which the poll below
        // observes — *before* running "done"'s own entry effects, one of
        // which is deleting this whole directory). Terminal removal is
        // checked separately, afterward, once this check is safely done.
        let yaml = r#"
name: worktree-snapshot-flow
worktree: true
stages:
  first:
    kind: shell
    command: "touch marker-first"
    on: { done: gate, error: failed }
  gate:
    kind: human_gate
    on: { resumed: second }
  second:
    kind: shell
    command: "touch marker-second"
    on: { done: verified, error: failed }
  verified:
    kind: human_gate
    on: { resumed: done }
  done:
    kind: terminal
  failed:
    kind: human_gate
    on: { resumed: done }
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());

        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), "unused");

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_stage(&pool, &task_id, "gate").await;

        let original_worktree = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
        assert!(original_worktree.join("marker-first").exists());

        // Mutate both config.cwd and the project's own name while the task
        // is parked at the gate — neither should affect where the next
        // stage runs.
        let other_repo = tempdir();
        tasks::merge_config(
            &pool,
            &task_id,
            json!({ "cwd": other_repo.to_string_lossy() }),
        )
        .await
        .unwrap();
        projects::rename(&pool, &project_id, "renamed")
            .await
            .unwrap();

        engine.advance(&task_id, &def, "resumed").await.unwrap();
        wait_until_stage(&pool, &task_id, "verified").await;

        assert!(
            original_worktree.join("marker-second").exists(),
            "the second stage must still run in the worktree start_task actually created"
        );

        // Terminal removal must target that same original worktree, not
        // one derived from the now-changed config/project. Driven as its
        // own transition, after the check above, so it can't race it.
        engine.advance(&task_id, &def, "resumed").await.unwrap();
        wait_until_stage(&pool, &task_id, "done").await;
        wait_until_task_status(&pool, &task_id, "closed").await;
        wait_until_path_gone(&original_worktree).await;
    }

    // ---- built-in coding-task workflow (P2-7, issue #18) -----------------

    /// Locates a sibling workspace binary next to this test binary
    /// (`target/<profile>/deps/<test-exe>` -> `target/<profile>/<name>`),
    /// same technique `tests/e2e_smoke.rs`'s own `workspace_binary` uses —
    /// duplicated rather than shared, since that's a separate integration
    /// test crate this `#[cfg(test)]` module can't import from.
    fn workspace_binary(name: &str) -> PathBuf {
        let mut path = std::env::current_exe().expect("test binary has no path");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.join(name)
    }

    /// Writes an executable script to `dir/name` with `contents`.
    fn write_script(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    /// A single fake `claude` binary standing in for *both* `coder` and
    /// `reviewer` — `engine_with_adapter` configures one binary for the
    /// whole engine (`ClaudeAdapter::spawn` always uses its own fixed
    /// `binary` field, never `RoleConfig.cli` — a role's `cli:` doesn't
    /// pick the executable), so distinguishing the two roles has to happen
    /// inside the script itself. `adapter/claude.rs::spawn` passes
    /// `--system-prompt <text>` whenever a role resolves one, and
    /// `coder-system.md`/`reviewer-system.md` open with distinct wording —
    /// the wrapper greps its own argv for that marker. Both roles need
    /// `MOCK_CLAUDE_ONESHOT` (neither `coding`/`revising` nor
    /// `internal_review` is open-ended, so each only concludes once its
    /// run goes idle); the reviewer's reply is read fresh from
    /// `reply_path` on every invocation, so a test can set it once up
    /// front and never needs to regenerate this script.
    fn role_dispatch_claude(dir: &Path, mock_claude: &Path, reply_path: &Path) -> PathBuf {
        write_script(
            dir,
            "mock-claude-role-dispatch.sh",
            &format!(
                r#"#!/bin/sh
set -eu
role="coder"
for arg in "$@"; do
    case "$arg" in
        *"reviewing agent"*) role="reviewer" ;;
    esac
done
export MOCK_CLAUDE_ONESHOT=1
if [ "$role" = "reviewer" ]; then
    export MOCK_CLAUDE_REPLY="$(cat "{reply_path}")"
else
    export MOCK_CLAUDE_REPLY="did the thing"
fi
exec "{mock_claude}" "$@"
"#,
                reply_path = reply_path.display(),
                mock_claude = mock_claude.display(),
            ),
        )
    }

    /// Prepends `dir` to `PATH` for the process, restoring the original
    /// value on drop.
    ///
    /// Mutating a whole test process's environment for one test is a
    /// theoretical race against every other test running concurrently in
    /// the same process — but this only ever prepends a stub named `gh`,
    /// and grepping this entire crate confirms no other test anywhere
    /// shells out to a bare `gh` command (every other `"gh ..."` string in
    /// the test suite is loader/template text that's only ever parsed or
    /// rendered, never executed), so the practical risk is nil. `unsafe`
    /// per Rust 2024's `std::env::set_var`, which exists for exactly this
    /// class of whole-process mutation.
    struct PathPrefixGuard {
        original: Option<std::ffi::OsString>,
    }

    impl PathPrefixGuard {
        fn new(dir: &Path) -> Self {
            let original = std::env::var_os("PATH");
            let mut new_path = std::ffi::OsString::from(dir);
            if let Some(existing) = &original {
                new_path.push(":");
                new_path.push(existing);
            }
            // SAFETY: see struct doc comment.
            unsafe { std::env::set_var("PATH", new_path) };
            PathPrefixGuard { original }
        }
    }

    impl Drop for PathPrefixGuard {
        fn drop(&mut self) {
            // SAFETY: see struct doc comment.
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var("PATH", value),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    /// A stub `gh` covering exactly the three invocations
    /// `coding-task.yaml` makes (`pr create`, `pr view` — with or without
    /// `--json reviewDecision`, `pr checks`), backed by real `git`/a real
    /// local bare repo for everything else. Returns the directory to
    /// prepend to `PATH`.
    fn gh_stub_dir(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "gh",
            r#"#!/bin/sh
set -eu
case "$1 $2" in
    "pr create")
        echo "https://example.test/pr/42"
        ;;
    "pr view")
        if printf '%s\n' "$@" | grep -q reviewDecision; then
            echo "APPROVED"
        else
            echo '{"number": 42, "url": "https://example.test/pr/42"}'
        fi
        ;;
    "pr checks")
        echo "SUCCESS"
        ;;
    *)
        echo "stub gh: unhandled subcommand: $*" >&2
        exit 1
        ;;
esac
"#,
        );
        dir.to_path_buf()
    }

    /// `scripts_dir` is owned by the caller, not this function — it has to
    /// outlive the whole test (every stage re-spawns the wrapper), and a
    /// `TempDir` created and dropped in here would delete it out from under
    /// later stages the moment this function returns.
    async fn seed_coding_task(
        pool: &SqlitePool,
        repo: &Path,
        scripts_dir: &Path,
        reviewer_reply: &str,
    ) -> (String, Arc<WorkflowDefinition>, PathBuf) {
        let def = Arc::new(
            WorkflowDefinition::load(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../workflows/coding-task.yaml")
                    .as_path(),
            )
            .expect("workflows/coding-task.yaml failed to load"),
        );

        let mock_claude = workspace_binary("mock-claude");
        assert!(
            mock_claude.exists(),
            "mock-claude binary not found at {mock_claude:?} \
             (run `cargo build --workspace --all-targets` first)"
        );
        let reply_path = scripts_dir.join("reviewer-reply.json");
        fs::write(&reply_path, reviewer_reply).unwrap();
        let claude_wrapper = role_dispatch_claude(scripts_dir, &mock_claude, &reply_path);

        let project_id = projects::create(pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "Add a small feature",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;

        (task_id, def, claude_wrapper)
    }

    #[tokio::test]
    async fn the_real_coding_task_workflow_walks_the_happy_path_to_done() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;
        let origin = tempdir();
        git(&origin, &["init", "-q", "--bare"]).await;
        git(
            &repo,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        )
        .await;

        let scripts_dir = tempdir();
        let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));

        let (task_id, def, claude_wrapper) = seed_coding_task(
            &pool,
            &repo,
            &scripts_dir,
            r#"{"outcome": "approved", "feedback": ""}"#,
        )
        .await;
        let engine = engine_with_adapter(pool.clone(), &claude_wrapper.to_string_lossy());

        engine
            .start_task(&task_id, &def, Some("Add a small feature"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "done").await;
        wait_until_task_status(&pool, &task_id, "closed").await;

        assert_eq!(
            stage_trail(&pool, &task_id)
                .await
                .into_iter()
                .map(|(stage, _)| stage)
                .collect::<Vec<_>>(),
            vec![
                "coding",
                "internal_review",
                "open_pr",
                "checks_polling",
                "awaiting_human_review",
                "done",
            ]
        );

        // Worktree cleanup (#58) still fires for the real shipped workflow.
        let worktree_dir = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
        wait_until_path_gone(&worktree_dir).await;
    }

    /// The coder/reviewer loop (not just the happy path) actually wires up
    /// end to end: every return path lands on `revising`, not `coding`
    /// (§ planning notes on #18), and the loop guard escalates rather than
    /// looping forever.
    #[tokio::test]
    async fn the_real_coding_task_workflow_escalates_after_the_loop_guard_trips() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;
        // No `origin` remote and no `gh` stub — this never reaches
        // `open_pr`, so neither is needed.
        let scripts_dir = tempdir();

        let (task_id, def, claude_wrapper) = seed_coding_task(
            &pool,
            &repo,
            &scripts_dir,
            r#"{"outcome": "changes_requested", "feedback": "needs more tests"}"#,
        )
        .await;
        let engine = engine_with_adapter(pool.clone(), &claude_wrapper.to_string_lossy());

        engine
            .start_task(&task_id, &def, Some("Add a small feature"))
            .await
            .unwrap();
        wait_until_stage(&pool, &task_id, "escalate_to_human").await;

        let trail: Vec<String> = stage_trail(&pool, &task_id)
            .await
            .into_iter()
            .map(|(stage, _)| stage)
            .collect();
        assert_eq!(
            trail.iter().filter(|s| s.as_str() == "coding").count(),
            1,
            "coding only ever runs once; every return path goes through revising: {trail:?}"
        );
        assert!(
            trail.iter().filter(|s| s.as_str() == "revising").count() >= 3,
            "expected at least 3 trips through revising before the loop guard tripped: {trail:?}"
        );
    }

    // ---- sandboxed permission bypass threads through end to end (#67) ----
    //
    // Everything on either side of this seam already has its own test: a
    // unit test hand-builds a `RoleConfig { sandboxed: true, .. }` and
    // checks `ClaudeAdapter::spawn` reacts to it, and another checks
    // `role_config::resolve` doesn't drop the value in between. Neither
    // proves the seam itself — that a *real* `worktree: true` workflow,
    // driven through the real engine, actually ends up with the flag on
    // the actual spawned subprocess's argv. `coding-task.yaml`'s own tests
    // exercise `worktree: true` but never assert on `--permission-mode`
    // either way, so they'd pass identically whether this seam worked or
    // not.

    #[tokio::test]
    async fn a_worktree_enabled_stage_sandboxes_its_spawn() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;

        let yaml = r#"
name: sandboxed-flow
worktree: true
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let project_id = projects::create(&pool, "demo").await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: &def.name,
                title: "T",
                config: json!({ "cwd": repo.to_string_lossy() }),
            },
        )
        .await
        .unwrap()
        .id;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();

        let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
        wait_until_events_contain(
            &pool,
            &run.id,
            "model=sonnet|system_prompt=<unset>|permission_mode=bypassPermissions",
        )
        .await;
    }

    /// The other half of the seam: a task whose workflow never opted into
    /// `worktree: true` must reach the subprocess with the flag genuinely
    /// absent — same real-engine, real-argv proof as above, just the other
    /// value of the one thing that differs (no `worktree: true`, no `cwd`
    /// pointing at a real repo at all, matching how a `chat`-shaped task
    /// actually runs).
    #[tokio::test]
    async fn a_non_worktree_stage_does_not_sandbox_its_spawn() {
        let pool = connect_in_memory().await.unwrap();

        let yaml = r#"
name: unsandboxed-flow
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();

        let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
        wait_until_events_contain(
            &pool,
            &run.id,
            "model=sonnet|system_prompt=<unset>|permission_mode=<unset>",
        )
        .await;
    }

    // ---- cancel (#69) ----

    /// A single-shot `agent_turn` that would auto-advance to a terminal
    /// stage the moment its turn completes.
    fn cancellable_turn_def() -> Arc<WorkflowDefinition> {
        let yaml = r#"
name: cancellable
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;
        Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
    }

    #[tokio::test]
    async fn cancel_marks_the_task_cancelled_and_kills_its_run() {
        let pool = connect_in_memory().await.unwrap();
        let def = cancellable_turn_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();
        let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;

        engine.cancel_task(&task_id).await.unwrap();

        assert_eq!(
            tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
            "cancelled"
        );
        for _ in 0..200 {
            let run = task_runs::get(&pool, &run.id).await.unwrap().unwrap();
            if run.end_reason == Some(TaskRunEndReason::Cancelled) {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("timed out waiting for the run to be recorded as cancelled");
    }

    /// Cancel deliberately leaves `current_stage` alone, so an operator can
    /// still see *where* a task was stopped. Collapsing it to a terminal
    /// stage would throw that away, and would also fire the terminal
    /// stage's own effects (`closed`, its `stage_entered` event) for a task
    /// that never actually got there.
    #[tokio::test]
    async fn cancel_leaves_the_task_in_the_stage_it_was_cancelled_in() {
        let pool = connect_in_memory().await.unwrap();
        let def = cancellable_turn_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();
        wait_until_run_for_stage(&pool, &task_id, "coding").await;

        engine.cancel_task(&task_id).await.unwrap();

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "coding");
    }

    /// End-to-end: a single-shot turn that completes after a cancel must
    /// not carry the task on to `finished`/`closed`.
    ///
    /// Note which layer this actually pins down. Two independent things
    /// stop it — the turn watcher's `Cancelled` arm, which returns before
    /// `finish_turn`, and `advance_from_stage`'s guard behind it — and the
    /// watcher wins the race in this scenario, so removing the guard alone
    /// does *not* make this test fail. That's deliberate defense in depth,
    /// not redundancy: the guard covers the ordering this test can't
    /// reproduce on demand, where a turn completes and `finish_turn` is
    /// already past the watcher when the cancel lands.
    /// `advance_refuses_a_cancelled_task` is what pins the guard itself,
    /// and it does fail without it.
    #[tokio::test]
    async fn a_cancelled_task_does_not_advance_when_its_turn_completes() {
        let pool = connect_in_memory().await.unwrap();
        let def = cancellable_turn_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_oneshot.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();
        wait_until_run_for_stage(&pool, &task_id, "coding").await;

        engine.cancel_task(&task_id).await.unwrap();

        // Long enough for the watcher (100ms poll) to have seen the turn
        // finish and tried to advance several times over.
        tokio::time::sleep(StdDuration::from_millis(600)).await;

        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(
            task.status, "cancelled",
            "a cancelled task must stay cancelled"
        );
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(
            state.current_stage, "coding",
            "the completed turn must not have advanced a cancelled task"
        );
    }

    /// Directly exercises the guard, independent of subprocess timing: an
    /// `advance` on a cancelled task is refused rather than transitioning.
    #[tokio::test]
    async fn advance_refuses_a_cancelled_task() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.cancel_task(&task_id).await.unwrap();

        let err = engine.advance(&task_id, &def, "resumed").await.unwrap_err();
        assert!(matches!(err, EngineError::TaskCancelled(_)));

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "gate");
    }

    /// The hole this closes: `send_message_or_resume` dispatches on stage
    /// *kind*, which cancel deliberately doesn't change. Without the
    /// `tasks.status` check, resuming this `human_gate` would advance a
    /// cancelled task; for a standing-open `agent_turn` it would go further
    /// and spawn a fresh subprocess from the persisted `session_id`,
    /// restarting the very process cancel just killed.
    #[tokio::test]
    async fn a_cancelled_task_refuses_further_messages() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let dir = tempdir();
        fs::write(
            dir.join("gated.yaml"),
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
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &dir,
        );

        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.cancel_task(&task_id).await.unwrap();

        let err = engine
            .send_message_or_resume(&task_id, "carry on")
            .await
            .unwrap_err();
        assert!(matches!(err, SendMessageOrResumeError::TaskCancelled));

        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "gate");
    }

    #[tokio::test]
    async fn cancelling_an_already_cancelled_task_is_rejected() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.cancel_task(&task_id).await.unwrap();

        let err = engine.cancel_task(&task_id).await.unwrap_err();
        assert!(matches!(err, CancelTaskError::NotCancellable(status) if status == "cancelled"));
    }

    /// A task that already reached its terminal stage finished on its own.
    /// Reporting success would claim the daemon stopped something it
    /// didn't, and would re-run the worktree removal for no reason.
    #[tokio::test]
    async fn cancelling_a_closed_task_is_rejected() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        engine.advance(&task_id, &def, "resumed").await.unwrap();
        wait_until_task_status(&pool, &task_id, "closed").await;

        let err = engine.cancel_task(&task_id).await.unwrap_err();
        assert!(matches!(err, CancelTaskError::NotCancellable(status) if status == "closed"));
    }

    #[tokio::test]
    async fn cancelling_an_unknown_task_is_rejected() {
        let pool = connect_in_memory().await.unwrap();
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        let err = engine.cancel_task("no-such-task").await.unwrap_err();
        assert!(matches!(err, CancelTaskError::NoSuchTask));
    }

    /// Two operators (or a double-clicked button) cancelling at once: the
    /// per-task lock plus the status re-read *inside* it mean exactly one
    /// wins and the other gets a conflict — rather than both passing the
    /// check and both going on to kill and remove the worktree.
    #[tokio::test]
    async fn concurrent_cancels_of_the_same_task_leave_exactly_one_winner() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));
        engine.start_task(&task_id, &def, None).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let task_id = task_id.clone();
            handles.push(tokio::spawn(
                async move { engine.cancel_task(&task_id).await },
            ));
        }

        let mut ok = 0;
        let mut conflicts = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(()) => ok += 1,
                Err(CancelTaskError::NotCancellable(_)) => conflicts += 1,
                Err(err) => panic!("unexpected error: {err}"),
            }
        }
        assert_eq!(ok, 1, "exactly one cancel should have taken effect");
        assert_eq!(conflicts, 3);
    }

    /// §5.5's "removed on reaching `done` (or task cancellation)" — the
    /// half that was never implemented until #69.
    #[tokio::test]
    async fn cancel_removes_a_worktree_enabled_tasks_worktree() {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;

        let yaml = r#"
name: wt-cancellable
worktree: true
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task_in(&pool, &def.name, &repo).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, Some("go")).await.unwrap();
        wait_until_run_for_stage(&pool, &task_id, "coding").await;

        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        let (wt_repo, wt_project) = worktree_snapshot(&task).expect("worktree snapshot recorded");
        let path = worktree::worktree_path(&wt_repo, wt_project, &task_id).unwrap();
        assert!(path.exists(), "the worktree should exist before cancelling");

        engine.cancel_task(&task_id).await.unwrap();

        wait_until_path_gone(&path).await;
    }

    /// Regression test for the interleaving that made `send_message` take
    /// the per-task lock.
    ///
    /// `send_message` resumes a session directly, without going through
    /// `enter_agent_turn`. While it did that outside the lock, a resume
    /// could be mid-spawn (`SessionSlot::Establishing`) exactly when a
    /// cancel ran: cancel would mark the task `cancelled`, then fail with
    /// `AlreadyStarting` — leaving a live agent attached to a task whose
    /// status makes every retry a 409, so nothing could ever kill it.
    ///
    /// Racing the two by scheduling doesn't reproduce it — the spawn
    /// window is a few microseconds wide and such a test passes either way
    /// — so this pins the *property* that closes it instead, the way
    /// `session.rs` pins its own ordering bugs by driving internals
    /// directly rather than hoping the scheduler cooperates: while a
    /// task's lock is held, `send_message` must wait for it.
    ///
    /// That is exactly what makes `Establishing` unreachable from
    /// `cancel_task`, and it fails without the fix — `send_message` sails
    /// past a held lock and reserves a session slot underneath the cancel.
    #[tokio::test]
    async fn send_message_waits_for_the_per_task_lock_a_cancel_holds() {
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
        fs::write(dir.join("chat.yaml"), yaml).unwrap();
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &fixture_binary("fake_claude.py"),
            &dir,
        );
        engine.start_task(&task_id, &def, Some("go")).await.unwrap();
        wait_until_run_for_stage(&pool, &task_id, "chatting").await;

        // Stands in for a cancel mid-flight: it holds exactly this lock
        // across its status write and its call into `SessionManager`.
        let lock = engine.lock_for_task(&task_id).await;
        let guard = lock.lock().await;

        let sender = {
            let engine = Arc::clone(&engine);
            let task_id = task_id.clone();
            tokio::spawn(async move { engine.send_message(&task_id, "again").await })
        };

        tokio::time::sleep(StdDuration::from_millis(150)).await;
        assert!(
            !sender.is_finished(),
            "send_message established a session while a cancel held the task lock — \
             the interleaving that strands an unkillable agent"
        );

        drop(guard);
        sender.await.unwrap().unwrap();
    }

    /// A `poll` holds its stage open for minutes or hours, and cancel
    /// deliberately leaves `current_stage` alone — so a cancelled task must
    /// not keep firing its command every interval until the deadline. The
    /// marker file counts attempts by appending to it.
    ///
    /// Two mechanisms stop it, as with
    /// `a_cancelled_task_does_not_advance_when_its_turn_completes`: the
    /// runner abort (which kills the loop outright) and the advisory
    /// `is_cancelled` check in `run_poll_stage` (which ends it at the next
    /// attempt). The abort alone is enough, so deleting the advisory check
    /// would not fail this test. The check still earns its place for a
    /// runner the registry somehow doesn't hold — and this test does fail
    /// if *both* are removed, which is the property that matters.
    #[tokio::test]
    async fn a_cancelled_task_stops_polling() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let marker = dir.join("attempts");
        let def = parsed_poll_def(
            &format!("printf x >> {} && echo PENDING", marker.display()),
            GREEN_OR_RED,
        );
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        // Let at least one attempt land, so the loop is genuinely running.
        for _ in 0..500 {
            if fs::metadata(&marker).is_ok_and(|m| m.len() >= 1) {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert!(fs::metadata(&marker).is_ok(), "the poll never ran at all");

        engine.cancel_task(&task_id).await.unwrap();
        let at_cancel = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);

        // Several intervals' worth: a still-running loop would add attempts.
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        let later = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            at_cancel, later,
            "a cancelled task kept polling: {at_cancel} attempts at cancel, {later} after"
        );
    }

    /// A `shell` stage owns no `task_run`, so killing the task's agent
    /// session doesn't reach it. Cancel has to abort the detached runner —
    /// otherwise the command keeps running in a worktree cancel is about
    /// to delete.
    #[tokio::test]
    async fn cancel_kills_a_running_shell_stages_command() {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let marker = dir.join("ticks");
        // Runs far longer than the test, appending as it goes, so "did it
        // actually stop?" is observable rather than inferred.
        let yaml = format!(
            r#"
name: long-shell
stages:
  building:
    kind: shell
    command: "for i in $(seq 1 200); do printf x >> {} ; sleep 0.05; done"
    on: {{ done: finished, error: finished }}
  finished:
    kind: terminal
"#,
            marker.display()
        );
        let def = Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        for _ in 0..500 {
            if fs::metadata(&marker).is_ok_and(|m| m.len() >= 2) {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert!(
            fs::metadata(&marker).is_ok(),
            "the shell command never started"
        );

        engine.cancel_task(&task_id).await.unwrap();
        // The kill happens when the aborted future drops; give it a beat.
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        let at_cancel = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);

        tokio::time::sleep(StdDuration::from_millis(500)).await;
        let later = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            at_cancel, later,
            "a cancelled task's shell command kept running: {at_cancel} ticks at cancel, {later} after"
        );
    }

    /// The registry backing that abort is keyed by task id, which is the
    /// shape this codebase's reviews keep finding leaks in — so a runner
    /// that finishes normally must drop its own entry rather than waiting
    /// for a cancel that may never come.
    #[tokio::test]
    async fn a_finished_shell_runner_leaves_no_entry_behind() {
        let pool = connect_in_memory().await.unwrap();
        let yaml = r#"
name: quick-shell
stages:
  building:
    kind: shell
    command: "true"
    on: { done: finished, error: finished }
  finished:
    kind: terminal
"#;
        let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_task_status(&pool, &task_id, "closed").await;

        for _ in 0..200 {
            let empty = engine
                .detached_runners
                .lock()
                .expect("detached_runners mutex poisoned")
                .is_empty();
            if empty {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("a finished shell runner left its registry entry behind");
    }

    /// `start_task` needs the same guard `advance_from_stage` has:
    /// `create_task` writes the task row and only then starts it, so a
    /// cancel can land in between — and starting anyway would create a
    /// worktree and spawn an agent for a task already marked cancelled.
    #[tokio::test]
    async fn start_task_refuses_a_task_that_was_already_cancelled() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        // Cancel before the task ever started: no workflow_state row yet,
        // which must not be an error (it's the "nothing to kill" case).
        engine.cancel_task(&task_id).await.unwrap();

        let err = engine.start_task(&task_id, &def, None).await.unwrap_err();
        assert!(matches!(err, EngineError::TaskCancelled(_)));
        assert!(
            workflow_state::get(&pool, &task_id)
                .await
                .unwrap()
                .is_none(),
            "a cancelled task must not have been started"
        );
    }

    /// A worktree-enabled task cancelled before it ever reached a stage
    /// that called `worktree::ensure` has no snapshot, so there is nothing
    /// to remove — and asking anyway would log an error about a worktree
    /// that was never created.
    #[tokio::test]
    async fn cancel_skips_worktree_removal_for_a_task_that_never_made_one() {
        let pool = connect_in_memory().await.unwrap();
        let def = human_gate_chain_def();
        let task_id = seed_task(&pool, &def.name).await;
        let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

        engine.start_task(&task_id, &def, None).await.unwrap();
        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert!(worktree_snapshot(&task).is_none());

        engine.cancel_task(&task_id).await.unwrap();

        assert_eq!(
            tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
            "cancelled"
        );
    }
}
