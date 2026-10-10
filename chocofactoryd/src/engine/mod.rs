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
//! loop_counters`, keyed by stage name to `{ count }`: `count` is how many
//! times in a row that stage has been left through its guarded outcome. It
//! resets when the stage resolves any other way, or when the task arrives
//! at the guard's `then:` stage (#106) — see
//! `bump_loop_counter`/`reset_loop_counter`/`clear_guards_escaping_to`.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chocofactory_core::models::{
    EventType, InFlight, Project, RetryMode, RetryOutcome, Session, SessionEndReason,
    SessionStatus, Task,
};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::adapter::{Registry, UnknownCliError};
use crate::config_root;
use crate::db::{events, projects, sessions, tasks, workflow_state};
use crate::global_config::{GlobalConfig, GlobalConfigError};
use crate::poll;
use crate::role_config::{self, RoleConfigError};
use crate::session::{SessionError, SessionKind, SessionManager};
use crate::shell;
use crate::template;
use crate::workflow_def::{
    Capture, ShellCommand, StageDef, StageKind, Watch, WorkflowDefError, WorkflowDefinition,
};
use crate::worktree::{self, WorktreeError};

mod gate;
mod parallel;
mod render;
mod runners;
mod shell_stage;
mod stage_capture;
mod sweep;
mod terminal;
mod turn;
mod watch;
pub use sweep::{ParkReport, PollSweepReport, RestartEffect, restart_effect};

#[cfg(test)]
pub(crate) use gate::reply_verdict;
#[cfg(test)]
use render::{MAX_ENV_VALUE_BYTES, render_command, render_env};
#[cfg(test)]
use shell_stage::{EVENT_OUTPUT_TAIL_BYTES, tail};
#[cfg(test)]
use stage_capture::derive_capture;
use stage_capture::merge_stage_capture;
#[cfg(test)]
use stage_capture::{
    MAX_CAPTURE_BYTES, outcome_from_report, sole_top_level_json_object, turn_outcome,
    unwrap_code_fence,
};
#[cfg(test)]
use sweep::agent_reason;
#[cfg(test)]
use turn::unverified_note;
use turn::{MAX_CONSECUTIVE_RESUMES, ResumeSession};
use watch::{TimedOutWatch, set_poll_window, timed_out_watch};
#[cfg(test)]
use watch::{poll_window_for, remaining_budget};

#[derive(Clone, Copy)]
struct StageEntry<'a> {
    task_id: &'a str,
    definition: &'a Arc<WorkflowDefinition>,
    stage_name: &'a str,
    stage_def: &'a StageDef,
    payload: &'a Value,
    input: Option<&'a str>,
    resume: Option<&'a ResumeSession>,
    /// Set only when the entry starts a parallel group's branch.
    branch: Option<parallel::BranchEntry<'a>>,
}

/// In-flight detached `shell`/`poll` runners, keyed by task id and then by
/// the runner id [`WorkflowEngine::spawn_registered_runner`] hands out (#69).
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

/// `tasks.status` for a task the engine gave up moving forward on its own
/// (X-4, issue #61) — a fourth value beyond `open`/`closed`/`cancelled`.
///
/// Distinct from `open`: an `open` task is either actively running a stage
/// or waiting on a human by *design* (a `human_gate`, a standing-open
/// `agent_turn`); a `stuck` task stopped somewhere the workflow never
/// intended it to stop — an outcome with no `on:` edge, a transition that
/// failed, a session that never started, a run the idle reaper force-closed
/// mid-turn, or a process that exited without completing. Distinct from
/// `cancelled`: cancelling is a human's deliberate choice to stop the task
/// for good, where `stuck` is the engine reporting its own failure to make
/// progress, and — unlike `cancelled` — is recoverable: `retry_task`
/// re-enters the current stage and reopens the task.
const TASK_STATUS_STUCK: &str = "stuck";
/// The status a task has while its workflow is running — the only status
/// the startup poll sweep (#52) considers.
const TASK_STATUS_OPEN: &str = "open";

pub struct WorkflowEngine {
    pool: SqlitePool,
    session_manager: Arc<SessionManager>,
    /// Serializes `advance()` calls per task (§ review on PR #35): without
    /// this, two callers racing to advance the same task's `workflow_state`
    /// (e.g. the turn-completion watcher and `send_message_or_resume`'s
    /// `human_gate` relay) could both read the same row and then both
    /// write, silently clobbering one call's `loop_counters`.
    task_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The directory of built-in workflows (#129): the daemon's regenerated
    /// private copy of the workflows embedded in its binary
    /// (`<config root>/.builtin-workflows/` in production), or a test's own
    /// directory in tests. Real files, so prompt and script references
    /// resolve next to the YAML exactly as for a repo workflow.
    builtin_dir: PathBuf,
    /// The old global `~/.config/chocofactory/workflows/` folder. Used only
    /// to load a pre-#88 task (`workflow_path` NULL) that was created
    /// against it; never consulted to resolve a new task.
    legacy_workflows_dir: Option<PathBuf>,
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
    /// Neither stage kind opens a `session`, so killing the task's agent
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
    ///
    /// Ownership invariant (#52): every spawner — `start_task`,
    /// `advance_from_stage`, `retry_task_locked` (via `enter_stage` →
    /// `spawn_poll_runner`/`spawn_shell_runner`) and the startup sweep
    /// `resume_interrupted_polls` — registers its slot while holding the
    /// task's `task_locks` entry. So "does this task have a runner?"
    /// (`has_detached_runner`) answered under that same lock cannot go stale
    /// before the caller's own spawn registers. That is the sweep's only
    /// ownership marker; there is deliberately no database lease.
    detached_runners: std::sync::Mutex<DetachedRunners>,
    /// Source of the ids keying `detached_runners`' inner maps. Only needs
    /// to be unique per task, but a single global counter is simpler than
    /// per-task numbering and just as correct.
    next_runner_id: AtomicU64,
    /// Set by `abort_all_detached_runners` before it drains: once set, no
    /// new detached runner may start (see `spawn_registered_runner`).
    runners_stopping: std::sync::atomic::AtomicBool,
    /// Wall-clock source for every `poll` budget computation (#52). Never
    /// `Instant`: it and tokio's timers stand still while the machine
    /// sleeps, so a `timeout:` measured on them is not calendar time.
    /// Injectable so tests can jump the clock without waiting.
    wall_clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Milliseconds one network call (`fetch`, `ls-remote`) may take while a
    /// new task's base is resolved. Defaults to
    /// `worktree::BASE_NETWORK_TIMEOUT`; a test shortens it.
    base_network_timeout_ms: AtomicU64,
}

#[derive(Debug)]
pub enum EngineError {
    /// A parallel group was entered without its branch state in the
    /// workflow payload. Defensive: no path produces it.
    GroupStateMissing {
        stage: String,
    },
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
    /// A `read_only` role's turn could not start because the worktree
    /// baseline could not be recorded (#172). Failing closed: a turn nobody
    /// could check afterwards must not run.
    ReadOnlyBaseline {
        stage: String,
        role: String,
        reason: String,
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
    /// stage's capture didn't actually carry, or a stage that finished
    /// without storing a capture — is not this: `template::render`/
    /// `render_command` substitute an empty string for that instead and
    /// report it via `record_unresolved_template_note` (#60), since there's
    /// no way for the loader to have caught it ahead of time. (A stage that
    /// simply hasn't run yet is substituted the same way but only logged.) Malformed
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
    /// A `poll` stage was entered without a usable `payload.poll_window`
    /// (#52): missing, or malformed. The window is stamped in the same write
    /// that moves the task into the stage, so this is an invariant
    /// violation, returned rather than papered over with a default budget.
    PollWindow {
        stage: String,
        reason: String,
    },
    /// The role's `cli` (or, on a resume, the session's recorded one) names
    /// no adapter in the registry. Nothing was created or spawned.
    UnknownCli(UnknownCliError),
    /// The role's adapter can't run the role as configured (for example omp
    /// with `memory: true`). Nothing was created or spawned.
    RoleRejected(String),
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
            EngineError::GroupStateMissing { stage } => write!(
                f,
                "parallel stage '{stage}' was entered without its branch state in the workflow \
                 payload"
            ),
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
            EngineError::ReadOnlyBaseline {
                stage,
                role,
                reason,
            } => write!(
                f,
                "could not record the worktree baseline for read-only role '{role}' in stage \
                 '{stage}': {reason}; the agent was not started"
            ),
            EngineError::InvalidPollPattern { stage, reason } => write!(
                f,
                "stage '{stage}' has a poll outcome pattern that does not compile: {reason}"
            ),
            EngineError::Template { stage, reason } => {
                write!(f, "stage '{stage}' could not render a template: {reason}")
            }
            EngineError::PollWindow { stage, reason } => {
                write!(f, "stage '{stage}' has no usable poll window: {reason}")
            }
            EngineError::UnknownCli(err) => write!(f, "{err}"),
            EngineError::RoleRejected(message) => write!(f, "{message}"),
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

impl EngineError {
    /// Whether this is one of the benign races a resumed `human_gate`'s
    /// failed `advance_from_stage` call must *not* treat as a wedge —
    /// another caller already resumed or cancelled the task concurrently,
    /// not a stage that failed to start. Used by
    /// `send_message_or_resume`'s `HumanGate` arm (issue #61) to decide
    /// when *not* to call `mark_stuck`, and by `api/error.rs`'s
    /// `SendMessageOrResumeError` → `ApiError` mapping to decide when the
    /// same error is a 409 rather than a 500 — a single production
    /// definition so the two can't drift apart.
    pub(crate) fn is_benign_resume_race(&self) -> bool {
        matches!(
            self,
            EngineError::UnknownOutcome { .. }
                | EngineError::TerminalStageHasNoTransitions(_)
                | EngineError::StageMovedOn { .. }
                | EngineError::TaskCancelled(_)
        )
    }
}

/// The one allowlist (`^[A-Za-z0-9_-]+$`) every `workflow_def` name is
/// checked against before it ever touches the filesystem (P1-8 LLD §2.8),
/// used by [`resolve_task_workflow`] and [`parse_builtin_ref`]. Not
/// the workflow loader's absolute-path/`..` blocklist
/// (`workflow_def.rs::resolve_file`/`fileref::resolve_relative`): `name` is
/// a single opaque identifier that arrives straight from an HTTP request
/// body (#9) or CLI arg (#10), materially less trusted than a relative path
/// written into a workflow file already sitting on disk — an allowlist
/// leaves no path syntax to reason about.
fn is_valid_workflow_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Where a workflow name resolved to at task creation (#129).
#[derive(Debug, PartialEq, Eq)]
enum ResolvedWorkflow {
    /// `<repo>/.chocofactory/workflows/<name>.yaml`; recorded by canonical path.
    Repo(PathBuf),
    /// `<builtin_dir>/<name>.yaml`; recorded as `builtin:<name>@<VERSION>`.
    Builtin(PathBuf),
}

/// A `workflow_def` name resolved against a task's *project* (#88, #129):
/// `<project.repo_path>/.chocofactory/workflows/<name>.yaml` first (when the
/// project has a repo), then the built-in of that name in `builtin_dir`.
/// The old global workflows folder is never consulted. Used only at
/// task-creation time (`create_task_from`) — every later reload goes through
/// `WorkflowEngine::load_task_workflow`, which reads the *recorded* value.
fn resolve_task_workflow(
    builtin_dir: &Path,
    project: &Project,
    name: &str,
) -> Result<ResolvedWorkflow, ResolveError> {
    if !is_valid_workflow_name(name) {
        return Err(ResolveError::InvalidName(name.to_string()));
    }
    let repo_candidate = project.repo_path.as_ref().map(|repo_path| {
        Path::new(repo_path)
            .join(".chocofactory")
            .join("workflows")
            .join(format!("{name}.yaml"))
    });
    if let Some(path) = &repo_candidate
        && path.is_file()
    {
        return Ok(ResolvedWorkflow::Repo(path.clone()));
    }
    let builtin = builtin_dir.join(format!("{name}.yaml"));
    if builtin.is_file() {
        return Ok(ResolvedWorkflow::Builtin(builtin));
    }
    Err(workflow_not_found(
        name,
        repo_candidate.as_deref(),
        builtin_dir,
    ))
}

/// Builds a [`ResolveError::NotFound`] naming the repo path that was tried
/// (if any) and the built-in workflows that exist.
fn workflow_not_found(
    name: &str,
    repo_candidate: Option<&Path>,
    builtin_dir: &Path,
) -> ResolveError {
    let mut names: Vec<String> = fs::read_dir(builtin_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                path.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    let looked = match repo_candidate {
        Some(path) => format!("looked in {}; ", path.display()),
        None => String::new(),
    };
    ResolveError::NotFound(format!(
        "no workflow named '{name}' ({looked}built-in workflows: {})",
        names.join(", ")
    ))
}

/// Prefix of a `tasks.workflow_path` that names a built-in rather than a file.
const BUILTIN_PREFIX: &str = "builtin:";

/// The `workflow_path` recorded for a task running a built-in (#129).
fn builtin_ref(name: &str) -> String {
    format!(
        "{BUILTIN_PREFIX}{name}@{}",
        chocofactory_core::version::VERSION
    )
}

/// The name in a `builtin:<name>@<anything>` record; `None` for anything
/// else, including a name that fails [`is_valid_workflow_name`].
pub(crate) fn parse_builtin_ref(s: &str) -> Option<&str> {
    let rest = s.strip_prefix(BUILTIN_PREFIX)?;
    let (name, _version) = rest.split_once('@')?;
    is_valid_workflow_name(name).then_some(name)
}

/// What a task should run: a workflow by name, or an explicit file (#129).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRef {
    Name(String),
    File(PathBuf),
}

#[derive(Debug)]
pub enum ResolveError {
    InvalidName(String),
    /// Neither the repo nor the built-ins had `<name>.yaml`. The message
    /// (built by [`workflow_not_found`]) says where it looked, so `Display`
    /// prints it verbatim.
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
            ResolveError::NotFound(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Reads `path` exactly once, hashes those exact bytes (SHA-256, lowercase
/// hex), and parses the same bytes as a workflow definition (issue #88).
///
/// Deliberately a single read: hashing and parsing from two separate reads
/// would let the recorded hash describe a different file than the one that
/// actually ran, if something rewrote `path` in between — a real window on
/// a shared repo checkout, not a hypothetical one.
///
/// This is also where every role's `cli:` is checked against `registry`,
/// the one funnel every engine load goes through. The registry is a
/// required argument, so no load path can skip the check.
pub(crate) fn load_workflow_file(
    path: &Path,
    registry: &Registry,
) -> Result<(WorkflowDefinition, String), WorkflowDefError> {
    let raw = fs::read_to_string(path).map_err(WorkflowDefError::Io)?;
    let sha256 = sha256_hex(raw.as_bytes());
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let definition = WorkflowDefinition::parse(&raw, base_dir)?;
    for (name, role) in &definition.roles {
        if let Some(cli) = &role.cli {
            registry
                .lookup(Some(name), cli)
                .map_err(WorkflowDefError::UnknownCli)?
                .validate_role(name, &role.isolation)
                .map_err(WorkflowDefError::RoleRejected)?;
        }
    }
    Ok((definition, sha256))
}

/// SHA-256 of `bytes`, lowercase hex — the shape recorded in
/// `tasks.workflow_sha256` and compared against in `GET /tasks/{id}`'s
/// `workflow_file_status`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// Why `WorkflowEngine::check_config_patch` refused a config patch.
#[derive(Debug)]
pub enum ConfigPatchError {
    /// A role's adapter can't run the role as the workflow defines it.
    Rejected(String),
    Db(sqlx::Error),
}

#[derive(Debug)]
pub enum CreateTaskError {
    /// The task config's `roles.<name>.cli` names no known adapter.
    UnknownCli(UnknownCliError),
    /// A role's adapter can't run the role as the workflow and the task
    /// config define it. Nothing was written.
    RoleRejected(String),
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// `WorkflowRef::File` was given a relative path; the daemon's working
    /// directory is meaningless to the caller, so it must be absolute.
    WorkflowFileNotAbsolute(PathBuf),
    /// `project_id` doesn't reference an existing project. Checked
    /// explicitly (P1-9 review) rather than left to surface as whatever
    /// `sqlx::Error` a raw `tasks.project_id` foreign-key violation
    /// produces — the same care `db::projects::delete`'s own caller
    /// already takes for the opposite direction of that same FK.
    NoSuchProject(String),
    Db(sqlx::Error),
    /// Canonicalizing the resolved workflow file's path failed (issue #88)
    /// — before it's recorded as `tasks.workflow_path`, which must be the
    /// canonical absolute path so a later `load_task_workflow` reload never
    /// depends on the daemon's current working directory. Carries the path
    /// that failed to canonicalize and the underlying I/O error.
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `start_task` failed for the task row this call just wrote (X-4,
    /// issue #61). Carries `task_id` — the caller only ever sees this
    /// `Err`, never the `Task` `start_task` was trying to start, so
    /// without it there'd be no way to find the now-`stuck` (or, per
    /// `create_task`'s own `TaskCancelled` carve-out, still-`open`) row
    /// this error is about.
    Start {
        task_id: String,
        source: EngineError,
    },
    /// The commit to fork the worktree from could not be resolved. Nothing
    /// was written.
    Base(worktree::BaseError),
    /// A base was given for a workflow that creates no worktree.
    BaseWithoutWorktree,
    /// The workflow creates a worktree but the task has no repo to fork it
    /// from. Nothing was written.
    NoRepo {
        workflow: String,
    },
}

impl fmt::Display for CreateTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateTaskError::UnknownCli(err) => write!(f, "{err}"),
            CreateTaskError::RoleRejected(message) => write!(f, "{message}"),
            CreateTaskError::Resolve(err) => write!(f, "{err}"),
            CreateTaskError::WorkflowDef(err) => write!(f, "{err}"),
            CreateTaskError::WorkflowFileNotAbsolute(path) => write!(
                f,
                "workflow file path '{}' must be absolute",
                path.display()
            ),
            CreateTaskError::NoSuchProject(id) => write!(f, "no such project '{id}'"),
            CreateTaskError::Db(err) => write!(f, "{err}"),
            CreateTaskError::Canonicalize { path, source } => write!(
                f,
                "could not resolve the canonical path of workflow file '{}': {source}",
                path.display()
            ),
            CreateTaskError::Start { task_id, source } => {
                write!(f, "task '{task_id}' failed to start: {source}")
            }
            CreateTaskError::Base(err) => write!(f, "{err}"),
            CreateTaskError::BaseWithoutWorktree => {
                write!(
                    f,
                    "--base only applies to a workflow that creates a worktree"
                )
            }
            CreateTaskError::NoRepo { workflow } => write!(
                f,
                "workflow '{workflow}' creates a worktree but the task has no repo: pass --repo or give the project a repo"
            ),
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
    /// The stage is open-ended, but no `session` has ever been recorded
    /// for it (e.g. `create_task`'s `start_task` failed before spawning
    /// one).
    NoOpenRun(String),
    /// The task was cancelled (#69). Detected under the per-task lock, so
    /// unlike `send_message_or_resume`'s own earlier check this one cannot
    /// be raced by a concurrent `cancel_task`.
    TaskCancelled,
    /// The task is `stuck` (X-4, issue #61) — detected under the per-task
    /// lock, the same way `TaskCancelled` is. Carries the task's own `id`
    /// (so the hint below can name it), its `stuck_reason`, and
    /// `can_retry` — whether the task even has a `workflow_state` row for
    /// `retry_task` to re-enter. A task marked stuck before one was ever
    /// created (`create_task`'s `start_task` failing before
    /// `workflow_state::create` runs) can only ever be cancelled; pointing
    /// its operator at `task retry` would just trade one 409 for another.
    TaskStuck {
        task_id: String,
        reason: String,
        can_retry: bool,
    },
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// This task's recorded `workflow_path` (issue #88) names a file that no
    /// longer exists. Deliberately not a fallback to a fresh name lookup —
    /// see `LoadTaskWorkflowError::MissingFile`.
    MissingWorkflowFile(PathBuf),
    /// The task's built-in workflow is not part of this daemon version.
    BuiltinWorkflowGone(String),
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
                write!(f, "stage '{stage}' has no session recorded for it yet")
            }
            SendMessageError::TaskCancelled => {
                write!(f, "task was cancelled and accepts no further messages")
            }
            SendMessageError::TaskStuck {
                task_id,
                reason,
                can_retry,
            } => {
                if *can_retry {
                    write!(
                        f,
                        "task is stuck: {reason}; run 'choco task retry {task_id}'"
                    )
                } else {
                    write!(
                        f,
                        "task is stuck: {reason}; run 'choco task cancel {task_id}'"
                    )
                }
            }
            SendMessageError::Resolve(err) => write!(f, "{err}"),
            SendMessageError::WorkflowDef(err) => write!(f, "{err}"),
            SendMessageError::MissingWorkflowFile(path) => write!(
                f,
                "this task's recorded workflow file is missing: {}",
                path.display()
            ),
            SendMessageError::BuiltinWorkflowGone(name) => write!(
                f,
                "the built-in workflow '{name}' is not part of this version of chocofactoryd"
            ),
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

impl From<LoadTaskWorkflowError> for SendMessageError {
    fn from(err: LoadTaskWorkflowError) -> Self {
        match err {
            LoadTaskWorkflowError::Resolve(err) => SendMessageError::Resolve(err),
            LoadTaskWorkflowError::WorkflowDef(err) => SendMessageError::WorkflowDef(err),
            LoadTaskWorkflowError::MissingFile(path) => SendMessageError::MissingWorkflowFile(path),
            LoadTaskWorkflowError::BuiltinGone(name) => SendMessageError::BuiltinWorkflowGone(name),
        }
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
    /// The task is `stuck` (X-4, issue #61), so it accepts no further
    /// messages or resume signals until `retry_task` reopens it. Carries
    /// the task's `id`, `stuck_reason`, and `can_retry`, same as
    /// `SendMessageError::TaskStuck`.
    TaskStuck {
        task_id: String,
        reason: String,
        can_retry: bool,
    },
    /// The gate reads its verdict from the reply (#175) and the reply has no
    /// marker line. Nothing was recorded or changed. `markers` are the gate's
    /// lines, in declared order.
    ReplyNeedsMarker {
        stage: String,
        markers: Vec<String>,
    },
    /// The reply carries markers for different outcomes (#175). Nothing was
    /// recorded or changed. `found` lists the marker lines present.
    ReplyHasConflictingMarkers {
        stage: String,
        found: Vec<String>,
    },
    /// The gate doesn't read markers and the reply is nothing but marker
    /// lines of other gates (#179): it would be taken as a note, not a
    /// verdict. Nothing was recorded or changed. `found` lists the distinct
    /// marker lines in the reply; `resumes_to` is where a note would send
    /// the task; `rewatch` says whether `retry` would watch again.
    ReplyIsOnlyMarkers {
        task_id: String,
        stage: String,
        found: Vec<String>,
        resumes_to: Option<String>,
        rewatch: bool,
    },
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// This task's recorded `workflow_path` (issue #88) names a file that no
    /// longer exists — see `LoadTaskWorkflowError::MissingFile`.
    MissingWorkflowFile(PathBuf),
    /// The task's built-in workflow is not part of this daemon version.
    BuiltinWorkflowGone(String),
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
            SendMessageOrResumeError::TaskStuck {
                task_id,
                reason,
                can_retry,
            } => {
                if *can_retry {
                    write!(
                        f,
                        "task is stuck: {reason}; run 'choco task retry {task_id}'"
                    )
                } else {
                    write!(
                        f,
                        "task is stuck: {reason}; run 'choco task cancel {task_id}'"
                    )
                }
            }
            SendMessageOrResumeError::ReplyNeedsMarker { stage, markers } => write!(
                f,
                "stage '{stage}' reads its verdict from your reply: put {} alone on its own \
                 line. Nothing was sent.",
                markers.join(" or ")
            ),
            SendMessageOrResumeError::ReplyHasConflictingMarkers { found, .. } => {
                let listed = match found.as_slice() {
                    [a, b] => format!("both {a} and {b}"),
                    [init @ .., last] => format!("{} and {last}", init.join(", ")),
                    [] => String::new(),
                };
                write!(f, "your reply has {listed}; keep one. Nothing was sent.")
            }
            SendMessageOrResumeError::ReplyIsOnlyMarkers {
                task_id,
                stage,
                found,
                resumes_to,
                rewatch,
            } => {
                write!(
                    f,
                    "stage '{stage}' doesn't read {}: a reply here is a note",
                    found.join(" or ")
                )?;
                if let Some(next) = resumes_to {
                    write!(f, ", and sends the task on to stage '{next}'")?;
                }
                write!(
                    f,
                    " \u{2014} not a verdict. Nothing was sent. Write the note in words, or cancel \
                     the task with 'choco task cancel {task_id}'."
                )?;
                if *rewatch {
                    write!(f, " To watch again, run 'choco task retry {task_id}'.")?;
                }
                Ok(())
            }
            SendMessageOrResumeError::Resolve(err) => write!(f, "{err}"),
            SendMessageOrResumeError::WorkflowDef(err) => write!(f, "{err}"),
            SendMessageOrResumeError::MissingWorkflowFile(path) => write!(
                f,
                "this task's recorded workflow file is missing: {}",
                path.display()
            ),
            SendMessageOrResumeError::BuiltinWorkflowGone(name) => write!(
                f,
                "the built-in workflow '{name}' is not part of this version of chocofactoryd"
            ),
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

impl From<LoadTaskWorkflowError> for SendMessageOrResumeError {
    fn from(err: LoadTaskWorkflowError) -> Self {
        match err {
            LoadTaskWorkflowError::Resolve(err) => SendMessageOrResumeError::Resolve(err),
            LoadTaskWorkflowError::WorkflowDef(err) => SendMessageOrResumeError::WorkflowDef(err),
            LoadTaskWorkflowError::MissingFile(path) => {
                SendMessageOrResumeError::MissingWorkflowFile(path)
            }
            LoadTaskWorkflowError::BuiltinGone(name) => {
                SendMessageOrResumeError::BuiltinWorkflowGone(name)
            }
        }
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

/// An `open` task parked at a gate because a watcher's timeout elapsed
/// (#179), as `choco task status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WatchTimedOutInfo {
    pub stage: String,
    pub timeout_secs: Option<u64>,
    pub resumes_to: Option<String>,
}

/// Errors from [`WorkflowEngine::retry_task`] (X-4, issue #61).
#[derive(Debug)]
pub enum RetryTaskError {
    NoSuchTask,
    /// The task isn't `stuck` — carries its actual status, the same way
    /// `CancelTaskError::NotCancellable` does.
    NotStuck(String),
    /// The task has no `workflow_state` row at all, so there is no current
    /// stage to re-enter — e.g. `create_task`'s `start_task` failed before
    /// `workflow_state::create` ever ran.
    NoWorkflowState,
    /// `workflow_state.current_stage` names a stage the (possibly
    /// re-resolved) workflow definition no longer declares.
    UnknownStage(String),
    /// The current stage's `session` is still `Active` — defensive: a
    /// `stuck` task's stage should have nothing running, since the engine
    /// only marks a task stuck once it has given up on that stage's run.
    /// Carries the stage name.
    RunStillActive(String),
    /// The current stage is a parallel group. Retrying one comes with a later
    /// version; until then the task stays stuck as it was.
    ParallelGroupRetryNotYet {
        stage: String,
    },
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// This task's recorded `workflow_path` (issue #88) names a file that no
    /// longer exists — see `LoadTaskWorkflowError::MissingFile`.
    MissingWorkflowFile(PathBuf),
    /// The task's built-in workflow is not part of this daemon version.
    BuiltinWorkflowGone(String),
    Db(sqlx::Error),
    /// Re-entering the stage itself failed. `retry_task` marks the task
    /// stuck again before returning this.
    Enter(EngineError),
    /// `RetryMode::Resume` was asked for, and this stage's last run cannot
    /// be resumed (#92). Carries why. Reported rather than quietly started
    /// fresh: an operator who typed `--resume` is making a claim about what
    /// happened, and silently doing the other thing would hide that the
    /// claim was wrong.
    NotResumable(String),
    /// `--resume` or `--fresh` at a gate a watcher's timeout parked the task
    /// at (#179): the retry re-watches, and there is no session to resume.
    RewatchTakesNoMode {
        task_id: String,
        stage: String,
    },
}

impl fmt::Display for RetryTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RetryTaskError::NoSuchTask => write!(f, "no such task"),
            RetryTaskError::NotStuck(status) => {
                write!(
                    f,
                    "task is '{status}', not 'stuck', so it cannot be retried"
                )
            }
            RetryTaskError::NoWorkflowState => write!(f, "task has no workflow_state row"),
            RetryTaskError::UnknownStage(stage) => {
                write!(f, "workflow_state references unknown stage '{stage}'")
            }
            RetryTaskError::ParallelGroupRetryNotYet { stage } => write!(
                f,
                "retrying parallel stage '{stage}' comes in a later version; cancel the task to \
                 stop it"
            ),
            RetryTaskError::RunStillActive(stage) => write!(
                f,
                "stage '{stage}' still has an active session; nothing to retry"
            ),
            RetryTaskError::Resolve(err) => write!(f, "{err}"),
            RetryTaskError::WorkflowDef(err) => write!(f, "{err}"),
            RetryTaskError::MissingWorkflowFile(path) => write!(
                f,
                "this task's recorded workflow file is missing: {}",
                path.display()
            ),
            RetryTaskError::BuiltinWorkflowGone(name) => write!(
                f,
                "the built-in workflow '{name}' is not part of this version of chocofactoryd"
            ),
            RetryTaskError::Db(err) => write!(f, "{err}"),
            RetryTaskError::Enter(err) => write!(f, "{err}"),
            RetryTaskError::NotResumable(why) => {
                write!(f, "this stage's last session cannot be resumed: {why}")
            }
            RetryTaskError::RewatchTakesNoMode { task_id, stage } => write!(
                f,
                "stage '{stage}' stopped watching; run 'choco task retry {task_id}' without \
                 --resume or --fresh to watch again (there is no session to resume)"
            ),
        }
    }
}

impl std::error::Error for RetryTaskError {}

impl From<LoadTaskWorkflowError> for RetryTaskError {
    fn from(err: LoadTaskWorkflowError) -> Self {
        match err {
            LoadTaskWorkflowError::Resolve(err) => RetryTaskError::Resolve(err),
            LoadTaskWorkflowError::WorkflowDef(err) => RetryTaskError::WorkflowDef(err),
            LoadTaskWorkflowError::MissingFile(path) => RetryTaskError::MissingWorkflowFile(path),
            LoadTaskWorkflowError::BuiltinGone(name) => RetryTaskError::BuiltinWorkflowGone(name),
        }
    }
}

impl From<sqlx::Error> for RetryTaskError {
    fn from(err: sqlx::Error) -> Self {
        RetryTaskError::Db(err)
    }
}

/// Errors from [`WorkflowEngine::load_task_workflow`] (issue #88) — every
/// place an *existing* task's workflow is reloaded (`send_message_locked`,
/// `send_message_or_resume`, `retry_task_locked`) shares this, rather than
/// each re-resolving `task.workflow_def` by name the way `create_task` does.
#[derive(Debug)]
pub enum LoadTaskWorkflowError {
    Resolve(ResolveError),
    WorkflowDef(WorkflowDefError),
    /// `task.workflow_path` names a file that no longer exists. Deliberately
    /// *not* a fallback to a fresh name lookup against the global workflows
    /// directory: the whole point of recording a path at creation time is
    /// that a task keeps running the exact file it started from, so
    /// silently substituting a different one here — even one with the same
    /// name — would defeat that guarantee. Carries the missing path so the
    /// error names it.
    MissingFile(PathBuf),
    /// The task runs a built-in (`builtin:<name>@<version>`) that this
    /// version of the daemon no longer ships.
    BuiltinGone(String),
}

impl fmt::Display for LoadTaskWorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadTaskWorkflowError::Resolve(err) => write!(f, "{err}"),
            LoadTaskWorkflowError::WorkflowDef(err) => write!(f, "{err}"),
            LoadTaskWorkflowError::MissingFile(path) => write!(
                f,
                "this task's recorded workflow file is missing: {}",
                path.display()
            ),
            LoadTaskWorkflowError::BuiltinGone(name) => write!(
                f,
                "the built-in workflow '{name}' is not part of this version of chocofactoryd"
            ),
        }
    }
}

impl std::error::Error for LoadTaskWorkflowError {}

/// Errors from [`WorkflowEngine::init_project_workflows`] (issue #88).
#[derive(Debug)]
pub enum InitWorkflowsError {
    NoSuchProject(String),
    /// The project has no `repo_path` at all — nowhere to seed into.
    NoRepoPath(String),
    /// The project has a `repo_path`, but it doesn't exist as a directory
    /// on this machine right now.
    RepoPathMissing(PathBuf),
    Db(sqlx::Error),
    Io(std::io::Error),
}

impl fmt::Display for InitWorkflowsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitWorkflowsError::NoSuchProject(id) => write!(f, "no such project '{id}'"),
            InitWorkflowsError::NoRepoPath(id) => {
                write!(f, "project '{id}' has no repo_path set")
            }
            InitWorkflowsError::RepoPathMissing(path) => write!(
                f,
                "project's repo_path '{}' does not exist or is not a directory",
                path.display()
            ),
            InitWorkflowsError::Db(err) => write!(f, "{err}"),
            InitWorkflowsError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for InitWorkflowsError {}

impl From<sqlx::Error> for InitWorkflowsError {
    fn from(err: sqlx::Error) -> Self {
        InitWorkflowsError::Db(err)
    }
}

/// What [`WorkflowEngine::mark_stuck`] did to the task's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StuckMark {
    /// The task was `open` and is now `stuck`.
    Marked,
    /// The task was no longer `open`; nothing changed.
    NotMarked,
    /// The write failed (logged); the task is as it was.
    WriteFailed,
}

impl WorkflowEngine {
    pub fn new(
        pool: SqlitePool,
        session_manager: Arc<SessionManager>,
        builtin_dir: PathBuf,
        global_config_path: Option<PathBuf>,
        events_notify: Arc<Notify>,
    ) -> Arc<Self> {
        Self::build(
            pool,
            session_manager,
            builtin_dir,
            global_config_path,
            events_notify,
            Arc::new(Utc::now),
        )
    }

    /// [`Self::new`] with an injected wall clock, so a test can jump time
    /// (a machine sleeping) without waiting.
    #[cfg(test)]
    fn new_with_clock(
        pool: SqlitePool,
        session_manager: Arc<SessionManager>,
        builtin_dir: PathBuf,
        global_config_path: Option<PathBuf>,
        events_notify: Arc<Notify>,
        wall_clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Arc<Self> {
        Self::build(
            pool,
            session_manager,
            builtin_dir,
            global_config_path,
            events_notify,
            wall_clock,
        )
    }

    fn build(
        pool: SqlitePool,
        session_manager: Arc<SessionManager>,
        builtin_dir: PathBuf,
        global_config_path: Option<PathBuf>,
        events_notify: Arc<Notify>,
        wall_clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            session_manager,
            task_locks: Mutex::new(HashMap::new()),
            builtin_dir,
            legacy_workflows_dir: None,
            global_config_path,
            events_notify,
            detached_runners: std::sync::Mutex::new(HashMap::new()),
            next_runner_id: AtomicU64::new(0),
            runners_stopping: std::sync::atomic::AtomicBool::new(false),
            wall_clock,
            base_network_timeout_ms: AtomicU64::new(
                worktree::BASE_NETWORK_TIMEOUT.as_millis() as u64
            ),
        })
    }

    /// Shortens the per-call network timeout used when resolving a new
    /// task's base, so a test can exercise the timeout without waiting.
    #[cfg(test)]
    pub(crate) fn set_base_network_timeout(&self, timeout: std::time::Duration) {
        self.base_network_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    fn base_network_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.base_network_timeout_ms.load(Ordering::SeqCst))
    }

    /// The current wall-clock time. Every `poll` budget computation goes
    /// through this (#52), never `Instant` or a bare `Utc::now()`.
    fn now(&self) -> DateTime<Utc> {
        (self.wall_clock)()
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

    /// Loads the workflow definition an *existing* task runs (issue #88) —
    /// the single place every reload of an already-created task's workflow
    /// goes through (`send_message_locked`, `send_message_or_resume`,
    /// `retry_task_locked`), so none of them re-resolve `task.workflow_def`
    /// by name the way `create_task` does.
    ///
    /// `task.workflow_path` is *the* authority once it's set: this loads
    /// exactly that file, never falling back to a fresh name lookup if it's
    /// missing — see `LoadTaskWorkflowError::MissingFile`'s doc comment for
    /// why a silent fallback would be worse than refusing. It does not
    /// verify the recorded `workflow_sha256`; the file is allowed to have
    /// changed since the task started (surfaced separately, for display
    /// only, by `GET /tasks/{id}`'s `workflow_file_status`), and this
    /// always loads and runs whatever the path currently contains.
    ///
    /// A `builtin:<name>@<version>` record loads the built-in directory's
    /// current `<name>.yaml` (#129); if this daemon no longer ships it that
    /// is `BuiltinGone`.
    ///
    /// `task.workflow_path` is `None` only for a task created before #88,
    /// which loads `task.workflow_def` from the legacy global folder when
    /// set and present, else from the built-ins.
    async fn load_task_workflow(
        &self,
        task: &Task,
    ) -> Result<WorkflowDefinition, LoadTaskWorkflowError> {
        match &task.workflow_path {
            Some(recorded) => {
                if let Some(name) = parse_builtin_ref(recorded) {
                    // A built-in follows the daemon: the *current* copy in
                    // the built-in directory, not the version it started on.
                    return match load_workflow_file(
                        &self.builtin_workflow_file(name),
                        self.registry(),
                    ) {
                        Ok((definition, _sha256)) => Ok(definition),
                        Err(WorkflowDefError::Io(io_err))
                            if io_err.kind() == std::io::ErrorKind::NotFound =>
                        {
                            Err(LoadTaskWorkflowError::BuiltinGone(name.to_string()))
                        }
                        Err(err) => Err(LoadTaskWorkflowError::WorkflowDef(err)),
                    };
                }
                let path = PathBuf::from(recorded);
                match load_workflow_file(&path, self.registry()) {
                    Ok((definition, _sha256)) => Ok(definition),
                    Err(WorkflowDefError::Io(io_err))
                        if io_err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Err(LoadTaskWorkflowError::MissingFile(path))
                    }
                    Err(err) => Err(LoadTaskWorkflowError::WorkflowDef(err)),
                }
            }
            None => {
                if !is_valid_workflow_name(&task.workflow_def) {
                    return Err(LoadTaskWorkflowError::Resolve(ResolveError::InvalidName(
                        task.workflow_def.clone(),
                    )));
                }
                let file_name = format!("{}.yaml", task.workflow_def);
                let legacy = self
                    .legacy_workflows_dir
                    .as_ref()
                    .map(|dir| dir.join(&file_name))
                    .filter(|path| path.is_file());
                let path = match legacy {
                    Some(path) => path,
                    None => {
                        let builtin = self.builtin_dir.join(&file_name);
                        if !builtin.is_file() {
                            return Err(LoadTaskWorkflowError::Resolve(workflow_not_found(
                                &task.workflow_def,
                                None,
                                &self.builtin_dir,
                            )));
                        }
                        builtin
                    }
                };
                load_workflow_file(&path, self.registry())
                    .map(|(definition, _sha256)| definition)
                    .map_err(LoadTaskWorkflowError::WorkflowDef)
            }
        }
    }

    /// The adapters this engine's sessions dispatch to; shared with
    /// validation so the two can't disagree.
    pub fn registry(&self) -> &Registry {
        self.session_manager.registry()
    }

    /// Checks a `PATCH` of a task's config before it is merged: every role
    /// the patch points at a CLI is asked whether that CLI can run the role
    /// as the task's workflow defines it. A task that doesn't exist passes
    /// (the merge that follows answers 404). A workflow that can't be loaded skips
    /// the check, since the turn-start check is the one that can't be
    /// skipped and fails closed. `Err` is the adapter's own message.
    pub async fn check_config_patch(
        &self,
        task_id: &str,
        patch: &Value,
    ) -> Result<(), ConfigPatchError> {
        let Some(task) = tasks::get(&self.pool, task_id)
            .await
            .map_err(ConfigPatchError::Db)?
        else {
            return Ok(());
        };
        match self.load_task_workflow(&task).await {
            Ok(definition) => {
                crate::adapter::check_task_config_roles(patch, &definition, self.registry())
                    .map_err(ConfigPatchError::Rejected)?;
            }
            Err(err) => {
                tracing::warn!(
                    task_id,
                    %err,
                    "skipping the role check on a config patch: the task's workflow didn't load"
                );
            }
        }
        Ok(())
    }

    /// Sets the old global workflows folder, used only to reload pre-#88
    /// tasks that have no recorded `workflow_path`.
    pub fn with_legacy_workflows_dir(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("with_legacy_workflows_dir must be called before the engine is shared")
            .legacy_workflows_dir = Some(dir);
        self
    }

    /// The built-in directory's file for built-in `name` (#129).
    pub fn builtin_workflow_file(&self, name: &str) -> PathBuf {
        self.builtin_dir.join(format!("{name}.yaml"))
    }

    /// Creates a task under `project_id` running the workflow called
    /// `workflow_def_name` — a thin wrapper over [`Self::create_task_from`].
    pub async fn create_task(
        self: &Arc<Self>,
        project_id: &str,
        workflow_def_name: &str,
        title: &str,
        initial_input: &str,
        config: Value,
    ) -> Result<Task, CreateTaskError> {
        self.create_task_from(
            project_id,
            WorkflowRef::Name(workflow_def_name.to_string()),
            title,
            initial_input,
            config,
            None,
        )
        .await
    }

    /// Creates a task under `project_id`, feeding `initial_input` in as the
    /// entry stage's first message (P1-8 LLD §2.7). `config` is the
    /// task-level override layer `role_config::resolve` reads.
    ///
    /// The workflow comes from (#129): an explicit absolute file
    /// (`WorkflowRef::File`), else the project's own
    /// `.chocofactory/workflows/<name>.yaml`, else the built-in of that
    /// name. The global `~/.config/chocofactory/workflows/` is never read.
    /// The definition is freshly loaded on every call. What is recorded on
    /// the task row (`workflow_path`/`workflow_sha256`, in the one insert,
    /// from one read of the file) is the canonical file path, or
    /// `builtin:<name>@<VERSION>` for a built-in; every later reload uses
    /// that record (`load_task_workflow`).
    ///
    /// For a workflow with `worktree: true` the commit the worktree forks
    /// from is resolved here, before anything is written
    /// (`worktree::resolve_base`): `base` if given, else the remote's default
    /// branch, freshly fetched. Both are recorded on the row by the one
    /// INSERT. A `base` for a workflow without a worktree is an error; such a
    /// workflow resolves and fetches nothing.
    pub async fn create_task_from(
        self: &Arc<Self>,
        project_id: &str,
        workflow: WorkflowRef,
        title: &str,
        initial_input: &str,
        mut config: Value,
        base: Option<&str>,
    ) -> Result<Task, CreateTaskError> {
        // Before any read or write: an unknown `cli` in the task's own
        // config is rejected without creating anything.
        crate::adapter::check_task_config_clis(&config, self.registry())
            .map_err(CreateTaskError::UnknownCli)?;

        // The project is loaded first, before any workflow resolution:
        // resolution needs it to search the project's own repo. Checked
        // explicitly rather than left to surface as a raw FK violation
        // from the `INSERT` below (P1-9 review).
        let project = projects::get(&self.pool, project_id)
            .await?
            .ok_or_else(|| CreateTaskError::NoSuchProject(project_id.to_string()))?;

        // Canonicalize *before* loading (issue #88 review, F2), so
        // `load_workflow_file`'s `path.parent()` is the exact directory
        // every later reload resolves prompt files against.
        let canonicalize = |path: &Path| {
            std::fs::canonicalize(path).map_err(|source| CreateTaskError::Canonicalize {
                path: path.to_path_buf(),
                source,
            })
        };
        let (definition, workflow_def_name, workflow_path_str, workflow_sha256) = match workflow {
            WorkflowRef::File(path) => {
                if !path.is_absolute() {
                    return Err(CreateTaskError::WorkflowFileNotAbsolute(path));
                }
                let canonical = canonicalize(&path)?;
                let (definition, sha) = load_workflow_file(&canonical, self.registry())
                    .map_err(CreateTaskError::WorkflowDef)?;
                let name = definition.name.clone();
                (
                    definition,
                    name,
                    canonical.to_string_lossy().into_owned(),
                    sha,
                )
            }
            WorkflowRef::Name(name) => {
                match resolve_task_workflow(&self.builtin_dir, &project, &name)
                    .map_err(CreateTaskError::Resolve)?
                {
                    ResolvedWorkflow::Repo(path) => {
                        let canonical = canonicalize(&path)?;
                        let (definition, sha) = load_workflow_file(&canonical, self.registry())
                            .map_err(CreateTaskError::WorkflowDef)?;
                        (
                            definition,
                            name,
                            canonical.to_string_lossy().into_owned(),
                            sha,
                        )
                    }
                    ResolvedWorkflow::Builtin(path) => {
                        let (definition, sha) = load_workflow_file(&path, self.registry())
                            .map_err(CreateTaskError::WorkflowDef)?;
                        let record = builtin_ref(&name);
                        (definition, name, record, sha)
                    }
                }
            }
        };
        // The roles the task's own config points at a different CLI: each
        // adapter has its say on the role as the workflow defines it, still
        // before anything is written.
        crate::adapter::check_task_config_roles(&config, &definition, self.registry())
            .map_err(CreateTaskError::RoleRejected)?;
        let definition = Arc::new(definition);

        // An explicit `--repo`/`config.cwd` always wins; this only fills in
        // the project's own repo when the caller didn't already say where
        // to run. `config` staying non-object (or already having a string
        // `cwd`) is left alone — the existing leniency elsewhere in this
        // module already treats a non-object `config` as "no overrides".
        if let (Some(repo_path), Some(map)) = (&project.repo_path, config.as_object_mut())
            && !matches!(map.get("cwd"), Some(Value::String(_)))
        {
            map.insert("cwd".to_string(), Value::String(repo_path.clone()));
        }

        // Resolved after every cheap check and before the INSERT, so a bad
        // base creates nothing. The row then carries the base from birth.
        let resolved_base = if !definition.worktree {
            if base.is_some() {
                return Err(CreateTaskError::BaseWithoutWorktree);
            }
            None
        } else {
            let repo = config.get("cwd").and_then(Value::as_str).ok_or_else(|| {
                CreateTaskError::NoRepo {
                    workflow: workflow_def_name.clone(),
                }
            })?;
            Some(
                worktree::resolve_base(Path::new(repo), base, self.base_network_timeout())
                    .await
                    .map_err(CreateTaskError::Base)?,
            )
        };

        let task = tasks::create(
            &self.pool,
            tasks::NewTask {
                project_id,
                workflow_def: &workflow_def_name,
                title,
                config,
                workflow_path: Some(&workflow_path_str),
                workflow_sha256: Some(&workflow_sha256),
                base_ref: resolved_base.as_ref().map(|b| b.base_ref.as_str()),
                base_commit: resolved_base.as_ref().map(|b| b.base_commit.as_str()),
            },
        )
        .await?;

        if let Err(err) = self
            .start_task(&task.id, &definition, Some(initial_input))
            .await
        {
            // A cancel landing in the window between the task row being
            // written and `start_task` running is not a stuck task — the
            // task is exactly as un-advanceable as an operator wanted it to
            // be, and marking it stuck would misreport why (X-4, issue
            // #61). Every other failure here — the entry stage's session
            // never starting, chief among them — means the engine gave up
            // moving the task forward, so it's reported the same way any
            // other stage-entry failure is.
            if !matches!(err, EngineError::TaskCancelled(_)) {
                // `start_task` writes `workflow_state` before it ever calls
                // `enter_stage` (review, X-4 round 2). A failure that
                // happens *before* that — `worktree::ensure` erroring on a
                // `worktree: true` workflow with no `config.cwd`, chiefly —
                // leaves no stage at all for `retry_task` to re-enter, so
                // the reason says so up front instead of pointing an
                // operator at a `choco task retry` that can only ever 409.
                let has_workflow_state = match workflow_state::get(&self.pool, &task.id).await {
                    Ok(state) => state.is_some(),
                    Err(db_err) => {
                        tracing::error!(
                            task_id = %task.id, %db_err,
                            "failed to check for a workflow_state row while recording why a \
                             task failed to start; assuming one exists"
                        );
                        true
                    }
                };
                let reason = if has_workflow_state {
                    format!("failed to start: {err}")
                } else {
                    format!(
                        "failed to start before reaching a stage: {err}; cannot be retried, \
                         cancel it"
                    )
                };
                // `enter_stage` already appends its own `Error` event for a
                // template failure — see `mark_stuck`'s doc comment.
                self.mark_stuck(
                    &task.id,
                    &reason,
                    matches!(err, EngineError::Template { .. }),
                )
                .await;
            }
            return Err(CreateTaskError::Start {
                task_id: task.id.clone(),
                source: err,
            });
        }

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
        // resume a *fresh* subprocess from the persisted `adapter_session_id` —
        // spawning an agent for a task the operator already cancelled,
        // and one that no retry could kill, since every later cancel is a
        // 409. The `human_gate` branch is safe only because
        // `advance_from_stage` re-checks under this same lock; this is
        // the matching check for the branch that resumes a session.
        if task.status == TASK_STATUS_CANCELLED {
            return Err(SendMessageError::TaskCancelled);
        }
        // Same reasoning as the cancelled check above, for the same reason
        // (X-4, issue #61): a stuck task has no live session worth resuming
        // a message into, and `choco task retry` is how it's expected to
        // move again — unless it never reached a stage at all, in which
        // case only `choco task cancel` can (see `TaskStuck::can_retry`).
        if task.status == TASK_STATUS_STUCK {
            // Best-effort, like the identical check in `create_task`: this
            // only decides which hint the error carries, so a transient DB
            // failure here must not turn an honest 409 into a 500 — the
            // task genuinely is stuck either way. Defaults to `true` (the
            // common case) rather than failing closed.
            let can_retry = match workflow_state::get(&self.pool, task_id).await {
                Ok(state) => state.is_some(),
                Err(err) => {
                    tracing::error!(
                        task_id, %err,
                        "failed to check for a workflow_state row while reporting a stuck \
                         task; assuming it can be retried"
                    );
                    true
                }
            };
            return Err(SendMessageError::TaskStuck {
                task_id: task.id.clone(),
                reason: task.stuck_reason.clone().unwrap_or_default(),
                can_retry,
            });
        }

        let definition = self.load_task_workflow(&task).await?;

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

        let session = sessions::get_current_for_stage(&self.pool, task_id, &current_stage)
            .await?
            .ok_or_else(|| SendMessageError::NoOpenRun(current_stage.clone()))?;

        let global = self
            .load_global_config()
            .map_err(SendMessageError::GlobalConfig)?;
        let cwd = working_dir(&task, &definition)?;
        // `stage_def.on` is checked empty just above (`StageNotOpenEnded`),
        // so this stage has no edges to route on — no outcomes to report.
        let resolved = role_config::resolve(
            role,
            role_def,
            &global,
            &task.config,
            cwd,
            definition.worktree,
            role_config::StageReport::default(),
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
            &session.id,
            EventType::HumanMessage,
            json!({ "text": text }),
        )
        .await
        {
            tracing::error!(session_id = %session.id, %err, "failed to record human message event");
        }

        self.session_manager
            .send_message(&session.id, text, &resolved.role_config)
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
        // the persisted `adapter_session_id`: restarting the very process the
        // operator just stopped. `tasks.status` is the only thing that
        // distinguishes that task from a healthy one here.
        if task.status == TASK_STATUS_CANCELLED {
            return Err(SendMessageOrResumeError::TaskCancelled);
        }
        // Same reasoning, for the same reason (X-4, issue #61): a stuck
        // task's stage has already given up, so there's no live session or
        // waiting `human_gate` to resume — only `choco task retry` moves it
        // again, unless it never reached a stage at all (see
        // `TaskStuck::can_retry`).
        if task.status == TASK_STATUS_STUCK {
            // Best-effort, same as `send_message_locked`'s identical check:
            // a transient DB failure here must not turn an honest 409 into
            // a 500 over a hint that's purely cosmetic.
            let can_retry = match workflow_state::get(&self.pool, task_id).await {
                Ok(state) => state.is_some(),
                Err(err) => {
                    tracing::error!(
                        task_id, %err,
                        "failed to check for a workflow_state row while reporting a stuck \
                         task; assuming it can be retried"
                    );
                    true
                }
            };
            return Err(SendMessageOrResumeError::TaskStuck {
                task_id: task.id.clone(),
                reason: task.stuck_reason.clone().unwrap_or_default(),
                can_retry,
            });
        }

        let definition = Arc::new(self.load_task_workflow(&task).await?);

        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(SendMessageOrResumeError::NoWorkflowState)?;
        let current_stage = state.current_stage.clone();

        let stage_def = definition
            .stages
            .get(&current_stage)
            .ok_or_else(|| SendMessageOrResumeError::UnknownStage(current_stage.clone()))?;

        match &stage_def.kind {
            StageKind::HumanGate { .. } => {
                self.reply_to_gate(task_id, &definition, &current_stage, text, &state.payload)
                    .await
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
    /// 1. The one fallible *read* (every `session` the task has had) comes
    ///    first, so a DB error here returns having changed nothing at all and
    ///    a retry starts clean.
    /// 2. `tasks.status` is written **second**, still inside the per-task
    ///    lock. Every guard that makes cancel stick — `advance_from_stage`,
    ///    `send_message`, `run_watch` — reads that column, so it has
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
    pub async fn cancel_task(
        self: &Arc<Self>,
        task_id: &str,
        keep: bool,
    ) -> Result<(), CancelTaskError> {
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
                self.cancel_task_locked(&task, keep).await
            }
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    /// The body of [`Self::cancel_task`], split out only so the per-task
    /// lock guard's scope stays obvious at the call site above.
    async fn cancel_task_locked(
        self: &Arc<Self>,
        task: &Task,
        keep: bool,
    ) -> Result<(), CancelTaskError> {
        let task_id = &task.id;

        // Step 1 — the fallible read, before anything is written or
        // killed, so a DB error here returns having changed nothing.
        //
        // Every run the task has had, not just the current stage's (#90). A
        // run's process can outlive the stage that started it — in #88 a
        // coder's CLI kept a background sub-agent running after its run was
        // recorded done and the task had moved on — and `SessionManager` finds
        // a live process by run id, whatever that run's recorded status.
        // A task that never reached a stage simply has no runs, which is the
        // state cancel wants; it may still own a worktree, removed below.
        let runs = sessions::list_for_task(&self.pool, task_id).await?;

        // Step 2 — the write every guard keys off, and the last thing here
        // that can fail. After this the task is durably cancelled: no stage
        // will advance, no message will be accepted, and a crash on the
        // very next line leaves a task that is visibly stopped rather than
        // one that silently looks healthy.
        //
        // `keep` (#102) is recorded by this same `UPDATE`: a second
        // statement could be lost to a crash, leaving a cancelled task whose
        // kept worktree nobody is told about.
        tasks::mark_cancelled(&self.pool, task_id, keep)
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
        for run in &runs {
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
            // Logged at `error` rather than propagated because this is an
            // invariant violation, not a runtime failure an operator can
            // act on. `AlreadyStarting` is the only error `cancel` returns,
            // and the lock argument above says it cannot happen; returning
            // it would dress a daemon bug up as a 409 the caller could
            // usefully retry. It could not, in any case: the status write
            // above has already committed, so a second cancel is refused
            // with `NotCancellable` whether or not this call returned
            // `Err`. If the lock invariant is ever broken this is the line
            // that says so.
            if let Err(err) = self.session_manager.cancel(&run.id).await {
                tracing::error!(
                    task_id, session_id = %run.id, %err,
                    "cancelled task's session could not be killed; a live agent may have been left running"
                );
            }
        }

        // An `agent_turn` is not the only thing that can be running. A
        // `shell` or `poll` stage runs detached, owns no `session` row,
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
        // The two are killed with different rigour, deliberately.
        // `abort_detached_runners` awaits each handle, so the command's
        // `ProcessGroup` guard is provably dropped before this line;
        // `SessionManager::cancel` only returns once `killpg` has, without
        // waiting for the agent to be reaped. That asymmetry is fine — a
        // `SIGKILL`ed process runs no further user-space code, so it cannot
        // write to the worktree after the signal lands — whereas an
        // *aborted future* has real teardown left to run, which is exactly
        // why that side is awaited.
        //
        // Gated on the snapshot rather than on `definition.worktree` so
        // this needs no workflow definition at all — and so a
        // worktree-enabled task cancelled before it ever reached a stage
        // that called `worktree::ensure` doesn't trip `remove_worktree`'s
        // "no snapshot to remove" error log for a worktree that was never
        // created.
        //
        // With `keep` neither the worktree nor the branch is touched: they
        // are handed to a person (#102). Otherwise the branch goes too,
        // pushed or not, once the worktree removal has succeeded.
        if !keep && worktree_snapshot(task).is_some() {
            if self.remove_worktree(task_id).await {
                self.cleanup_branch(task_id, false).await;
            } else {
                self.note_branch_left_in_place(task_id).await;
            }
        }
        Ok(())
    }

    /// Marks `task_id` `stuck` with `reason` (X-4, issue #61) — best-effort,
    /// like every other timeline write in this file: there is nothing left
    /// to return a failure to by the time any of this module's call sites
    /// reach here.
    ///
    /// `reason` should name the stage and say what happened, since it's the
    /// only thing a human sees in `choco task status` explaining why the
    /// task stopped moving. Uses `db::tasks::mark_stuck`'s compare-and-set —
    /// only an `open` task actually changes — so a late failure racing a
    /// concurrent `cancel_task`/`retry_task` can never clobber whatever
    /// status that other caller already settled on.
    ///
    /// `event_already_recorded` is `true` only when the caller is forwarding
    /// an `EngineError::Template` it got back from `enter_stage`: that path
    /// already appends its own stage-scoped `Error` event before returning
    /// the error (review, X-4 round 2), so appending a second one here with
    /// `stuck: true` would double the same failure up on the timeline. The
    /// status write below still happens either way — only the event is
    /// skipped.
    ///
    /// Returns what happened to the status: see [`StuckMark`].
    async fn mark_stuck(
        &self,
        task_id: &str,
        reason: &str,
        event_already_recorded: bool,
    ) -> StuckMark {
        match tasks::mark_stuck(&self.pool, task_id, reason).await {
            Ok(true) => {
                tracing::error!(task_id, reason, "task stuck: {reason}");
                if event_already_recorded {
                    return StuckMark::Marked;
                }
                let stage = match workflow_state::get(&self.pool, task_id).await {
                    Ok(Some(state)) => json!(state.current_stage),
                    Ok(None) => Value::Null,
                    Err(err) => {
                        tracing::error!(
                            task_id, %err,
                            "failed to read workflow_state while recording a stuck task's event"
                        );
                        Value::Null
                    }
                };
                match events::append_for_task(
                    &self.pool,
                    task_id,
                    EventType::Error,
                    json!({ "stage": stage, "message": reason, "stuck": true }),
                )
                .await
                {
                    Ok(_) => self.events_notify.notify_waiters(),
                    Err(err) => tracing::error!(
                        task_id, %err,
                        "failed to record a stuck-task event"
                    ),
                }
                StuckMark::Marked
            }
            // The task was no longer `open` — already cancelled, closed, or
            // already stuck — so there is nothing to mark. Not an error:
            // this is the compare-and-set doing exactly its job.
            Ok(false) => {
                tracing::info!(
                    task_id,
                    reason,
                    "task not marked stuck: it was no longer open"
                );
                StuckMark::NotMarked
            }
            Err(err) => {
                tracing::error!(
                    task_id, reason, %err,
                    "failed to mark task stuck"
                );
                StuckMark::WriteFailed
            }
        }
    }

    /// The stage a catch-all transition failure should be blamed on and
    /// retried against (X-4 review, round 2).
    ///
    /// `advance_from_stage` commits `workflow_state.current_stage` to the
    /// *next* stage, via `workflow_state::update`, strictly before it calls
    /// `enter_stage` on that next stage — so when `enter_stage` then fails
    /// (a session that won't start, chief among them), the task is already
    /// sitting in a stage different from the one whose `finish_*` caller is
    /// reporting the failure. Naming `from_stage` in that reason would point
    /// `choco task status` and a retry at the wrong stage: the one that
    /// finished cleanly, not the one that never started.
    ///
    /// Reads `workflow_state` back to tell the two cases apart: unchanged
    /// (or unreadable) means the failure was in the transition itself, so
    /// `from_stage` is still right; a different value means the *next*
    /// stage is the one that failed to start, and that's what gets blamed.
    /// The read failing degrades to `from_stage` rather than blocking
    /// `mark_stuck` — this only decides which stage name goes in the
    /// reason, and a stale-but-present reason beats none at all.
    async fn stage_to_blame(&self, task_id: &str, from_stage: &str) -> String {
        match workflow_state::get(&self.pool, task_id).await {
            Ok(Some(state)) => state.current_stage,
            Ok(None) => from_stage.to_string(),
            Err(err) => {
                tracing::warn!(
                    task_id, from_stage, %err,
                    "could not read workflow_state to find which stage a transition failure \
                     belongs to; blaming the stage that just finished"
                );
                from_stage.to_string()
            }
        }
    }

    /// Re-runs `task_id`'s current stage from scratch (X-4, issue #61) —
    /// the recovery path for a `stuck` task. Not a replay of whatever
    /// outcome tripped it into `stuck`: the engine never persisted one, and
    /// every stage kind can be re-entered cleanly, so this simply re-enters
    /// the current stage the same way any other transition does. Not an
    /// `on:` transition either — `loop_counters` are left untouched, and
    /// the timeline shows `X --[retry]--> X` rather than a hop to a
    /// different stage.
    ///
    /// Takes the same per-task lock `cancel_task` does, for the whole
    /// operation, and for the same reason: every fallible check has to see
    /// a consistent snapshot, and the write that reopens the task has to be
    /// ordered against a concurrent `cancel_task`/another `retry_task`
    /// without either racing the other.
    pub async fn retry_task(
        self: &Arc<Self>,
        task_id: &str,
        mode: RetryMode,
    ) -> Result<RetryOutcome, RetryTaskError> {
        let lock = self.lock_for_task(task_id).await;
        let result = {
            let _guard = lock.lock().await;
            self.retry_task_locked(task_id, mode).await
        };
        self.evict_task_lock_if_unshared(task_id, &lock).await;
        result
    }

    /// The body of [`Self::retry_task`], split out only so the guard's
    /// scope stays obvious at the call site above — the same split
    /// `cancel_task`/`cancel_task_locked` use.
    async fn retry_task_locked(
        self: &Arc<Self>,
        task_id: &str,
        mode: RetryMode,
    ) -> Result<RetryOutcome, RetryTaskError> {
        // Step 1 — every fallible read, before anything is written. A
        // failure here returns having changed nothing, so a retry of the
        // retry starts clean.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(RetryTaskError::NoSuchTask)?;
        if task.status != TASK_STATUS_STUCK && task.status != TASK_STATUS_OPEN {
            return Err(RetryTaskError::NotStuck(task.status));
        }

        let definition = Arc::new(self.load_task_workflow(&task).await?);

        let state = workflow_state::get(&self.pool, task_id)
            .await?
            .ok_or(RetryTaskError::NoWorkflowState)?;
        let current_stage = state.current_stage.clone();

        // An open task is retryable only when a watcher's timeout parked it
        // at a gate (#179): then retry watches again.
        if task.status == TASK_STATUS_OPEN {
            let Some(timed_out) = timed_out_watch(&definition, &current_stage, &state.payload)
            else {
                return Err(RetryTaskError::NotStuck(task.status));
            };
            if mode != RetryMode::Auto {
                return Err(RetryTaskError::RewatchTakesNoMode {
                    task_id: task_id.to_string(),
                    stage: timed_out.from,
                });
            }
            return self
                .rewatch_locked(task_id, &definition, &current_stage, &state, timed_out)
                .await;
        }

        let stage_def = definition
            .stages
            .get(&current_stage)
            .ok_or_else(|| RetryTaskError::UnknownStage(current_stage.clone()))?;

        // Fails closed until retrying a group exists: refused in step 1,
        // before any write, so the task keeps its stuck reason.
        if matches!(stage_def.kind, StageKind::Parallel { .. }) {
            return Err(RetryTaskError::ParallelGroupRetryNotYet {
                stage: current_stage,
            });
        }

        // Defensive, not a case any path today produces: the engine only
        // marks a task stuck once it has given up on its current stage's
        // run, so there should be nothing left active to collide with a
        // freshly re-entered one. Checked anyway rather than assumed,
        // since re-entering over a live run would start a second one
        // alongside it instead of replacing it.
        let last_session =
            sessions::get_current_for_stage(&self.pool, task_id, &current_stage).await?;
        if let Some(session) = &last_session
            && session.status == SessionStatus::Active
        {
            return Err(RetryTaskError::RunStillActive(current_stage));
        }

        // Still step 1: decide resume-or-fresh from reads only, so a
        // `NotResumable` below returns before anything has been written.
        // The whole decision — the run's end reason and how long its resume
        // chain already is — is read under the per-task lock this function
        // holds, and the run it describes is terminal by now (the check
        // above rejects an active one), so nothing can move under it
        // between deciding and re-entering the stage.
        let resumable = match mode {
            RetryMode::Fresh => Err("a fresh start was asked for".to_string()),
            RetryMode::Auto | RetryMode::Resume => {
                self.resumable_session(&task, &definition, stage_def, last_session.as_ref(), false)
                    .await?
            }
        };
        let (resume, fresh_reason) = match (mode, resumable) {
            (RetryMode::Resume, Err(why)) => return Err(RetryTaskError::NotResumable(why)),
            (_, Err(why)) => {
                tracing::info!(task_id, stage = %current_stage, why, "retrying with a fresh session");
                (None, Some(why))
            }
            (_, Ok(resume)) => (Some(resume), None),
        };

        // Step 2 — the reopen, which must land *before* the re-entry below.
        // A re-entered shell/turn can fail again quickly, and `mark_stuck`'s
        // compare-and-set only succeeds against `open`; if this reopen came
        // *after* re-entering, a fast second failure's `mark_stuck` would
        // find the task still `stuck` from the first failure, do nothing,
        // and silently drop the new failure — leaving the task showing its
        // stale original reason while nothing is actually running.
        //
        // `None` means the status changed under us since step 1's read —
        // impossible while this function holds the per-task lock unless
        // that invariant is broken, but reported as `NotStuck` rather than
        // assumed unreachable. Re-reads the task for its *actual* current
        // status rather than reusing the stale `"stuck"` this function
        // already knows is wrong (that would render as the
        // self-contradictory "task is 'stuck', not 'stuck', so it cannot
        // be retried"). If the row is gone entirely, that's `NoSuchTask` —
        // the same error step 1 would have returned had it read this late —
        // rather than a fabricated status no `Task` ever actually has.
        //
        // A retried poll gets a fresh budget — retry re-runs the stage from
        // scratch — so its window is re-stamped first, through the single
        // `workflow_state::update` below, which changes nothing else
        // (`current_stage`, `loop_counters`, `arrival` all come through
        // untouched). It lands *before* the reopen so a failed write leaves
        // the task `stuck`, unchanged.
        // For a stage that is no longer a poll, the same call removes a stale
        // window (so a later restart can't mistake it for an interrupted
        // poll); nothing is written when the payload comes out unchanged.
        let mut new_payload = state.payload.clone();
        set_poll_window(&mut new_payload, &definition, &current_stage, self.now())
            .map_err(RetryTaskError::Enter)?;
        let kind = stage_def.kind.name();
        let payload = if new_payload != state.payload || state.stage_kind.as_deref() != Some(kind) {
            let payload = new_payload;
            workflow_state::update(
                &self.pool,
                task_id,
                workflow_state::WorkflowStateUpdate {
                    current_stage: current_stage.clone(),
                    stage_kind: kind.to_string(),
                    loop_counters: state.loop_counters.clone(),
                    payload,
                    // A retry re-runs the stage without re-entering it.
                    enters_stage: false,
                },
            )
            .await?
            .ok_or(RetryTaskError::NoWorkflowState)?
            .payload
        } else {
            state.payload.clone()
        };
        if tasks::reopen_stuck(&self.pool, task_id).await?.is_none() {
            return Err(match tasks::get(&self.pool, task_id).await? {
                Some(t) => RetryTaskError::NotStuck(t.status),
                None => RetryTaskError::NoSuchTask,
            });
        }

        // Step 3 — work out a prompt_file-less agent_turn's input the same
        // way `start_task`/`advance_from_stage` do (P2-7a's
        // `payload.task.input`), then re-enter the stage directly —
        // `enter_stage`, not `advance`/`advance_from_stage`: this function
        // already holds the per-task lock those acquire themselves, and
        // `tokio::sync::Mutex` is not reentrant (see this module's header).
        // `entered_via: Some("retry")` is what makes the timeline show
        // `X --[retry]--> X` rather than reading as a fresh entry.
        let input = match &stage_def.kind {
            StageKind::AgentTurn {
                prompt_file: None, ..
            } => state
                .payload
                .get("task")
                .and_then(|task| task.get("input"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };

        let entered_via = match &resume {
            Some(_) => "retry_resume",
            None => "retry",
        };
        if let Err(err) = self
            .enter_stage(
                task_id,
                &definition,
                &current_stage,
                input.as_deref(),
                Some(entered_via),
                &payload,
                resume.as_ref(),
            )
            .await
        {
            // `enter_stage` already appends its own `Error` event for a
            // template failure — see `mark_stuck`'s doc comment.
            self.mark_stuck(
                task_id,
                &format!("stage '{current_stage}': retry failed: {err}"),
                matches!(err, EngineError::Template { .. }),
            )
            .await;
            return Err(RetryTaskError::Enter(err));
        }
        Ok(RetryOutcome {
            stage: current_stage,
            resumed: resume.is_some(),
            adapter_session_id: resume.map(|resume| resume.adapter_session_id),
            fresh_reason,
            rewatched: false,
        })
    }

    /// Watches again after a watcher's timeout (#179): moves the task back
    /// to the watcher stage `timed_out.from` with a fresh window, in one
    /// state write, and re-enters it. Runs under the per-task lock
    /// `retry_task` holds, on the state it read under that lock. The task
    /// stays `open`.
    async fn rewatch_locked(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        gate: &str,
        state: &chocofactory_core::models::WorkflowState,
        timed_out: TimedOutWatch,
    ) -> Result<RetryOutcome, RetryTaskError> {
        let from = timed_out.from;
        let from_def = definition
            .stages
            .get(&from)
            .ok_or_else(|| RetryTaskError::UnknownStage(from.clone()))?;
        let mut payload = state.payload.clone();
        // The record must stay true: the task is no longer at the gate it
        // arrived at by timing out.
        set_arrival(&mut payload, gate, "retry");
        set_poll_window(&mut payload, definition, &from, self.now())
            .map_err(RetryTaskError::Enter)?;
        let updated = workflow_state::update(
            &self.pool,
            task_id,
            workflow_state::WorkflowStateUpdate {
                current_stage: from.clone(),
                stage_kind: from_def.kind.name().to_string(),
                loop_counters: state.loop_counters.clone(),
                payload,
                enters_stage: true,
            },
        )
        .await?
        .ok_or(RetryTaskError::NoWorkflowState)?;
        if let Err(err) = self
            .enter_stage(
                task_id,
                definition,
                &from,
                None,
                Some("retry"),
                &updated.payload,
                None,
            )
            .await
        {
            self.mark_stuck(
                task_id,
                &format!("stage '{from}': retry failed: {err}"),
                matches!(err, EngineError::Template { .. }),
            )
            .await;
            return Err(RetryTaskError::Enter(err));
        }
        Ok(RetryOutcome {
            stage: from,
            resumed: false,
            adapter_session_id: None,
            fresh_reason: None,
            rewatched: true,
        })
    }

    /// For an `open` task parked at a gate because a watcher timed out:
    /// which stage stopped watching, after how long, and where a note goes.
    /// Informational: a workflow that won't load gives `None`.
    pub async fn watch_timed_out(
        &self,
        task: &Task,
        state: &chocofactory_core::models::WorkflowState,
    ) -> Option<WatchTimedOutInfo> {
        if task.status != TASK_STATUS_OPEN {
            return None;
        }
        let definition = match self.load_task_workflow(task).await {
            Ok(definition) => definition,
            Err(err) => {
                tracing::warn!(task_id = %task.id, %err, "could not load the workflow to describe a timed-out watcher");
                return None;
            }
        };
        let timed_out = timed_out_watch(&definition, &state.current_stage, &state.payload)?;
        Some(WatchTimedOutInfo {
            stage: timed_out.from,
            timeout_secs: timed_out.timeout.map(|t| t.as_secs()),
            resumes_to: timed_out.resumes_to,
        })
    }

    /// The branches of the task's current parallel group, for
    /// `GET /tasks/{id}`. Read-only, and unlike `watch_timed_out` it does not
    /// filter on task status: a stuck or cancelled task shows its branches.
    /// A workflow that won't load degrades to name order with no kinds.
    pub(crate) async fn branch_statuses(
        &self,
        task: &Task,
        state: &chocofactory_core::models::WorkflowState,
    ) -> Vec<chocofactory_core::models::BranchStatus> {
        let stage_kind = state.stage_kind.as_deref();
        if parallel::current_group_branches(&state.payload, &state.current_stage, stage_kind)
            .is_none()
        {
            return Vec::new();
        }
        let definition = match self.load_task_workflow(task).await {
            Ok(definition) => Some(definition),
            Err(err) => {
                tracing::warn!(task_id = %task.id, %err, "could not load the workflow to describe a parallel group's branches");
                None
            }
        };
        parallel::branch_statuses_from(
            &state.payload,
            &state.current_stage,
            stage_kind,
            definition.as_ref(),
        )
    }

    /// Whether the stuck stage's last run can be picked up where it left
    /// off (#92), or a sentence saying why not.
    ///
    /// Resumable means all of: the stage is an `agent_turn` that can
    /// conclude on its own (nothing else has a turn to resume — a standing
    /// chat session is picked up by sending it a message instead), the
    /// session recorded an `adapter_session_id`, its turn ended for a
    /// reason that describes something done *to* it — a usage limit, or
    /// the idle reaper's close — and it has not already been resumed
    /// [`MAX_CONSECUTIVE_RESUMES`] times in a row.
    ///
    /// Everything else starts fresh, and deliberately so: `no_report`,
    /// `lingered` and a plain crash are the agent's own failure, and
    /// resuming those is precisely the "resumed straight back into the same
    /// crash" loop that `SessionError::NotResumable` exists to prevent.
    async fn resumable_session(
        &self,
        task: &Task,
        definition: &WorkflowDefinition,
        stage_def: &StageDef,
        last_session: Option<&Session>,
        // A parallel branch's `on:` is always empty, but it is not a standing
        // session: it is single-shot and resumes like any other turn.
        is_branch: bool,
    ) -> Result<Result<ResumeSession, String>, RetryTaskError> {
        if !matches!(stage_def.kind, StageKind::AgentTurn { .. }) {
            return Ok(Err(
                "it is not an agent turn, so it has no session".to_string()
            ));
        }
        // A standing stage (empty `on:`, chat) never has a turn to resume:
        // its session stays open for further live messages, and
        // `send_message_or_resume` is what picks it up again. Resuming one
        // here would also hand it a prompt telling it to `report_outcome`,
        // which such a stage has no outcomes for. Unreachable today — a
        // standing stage gets no turn watcher and so is never marked stuck
        // by one — and checked anyway, since the cost of being wrong is a
        // turn instructed to do something it cannot do.
        if !is_branch && stage_def.on.is_empty() {
            return Ok(Err(
                "it is a standing session, which is resumed by sending it a message \
                 rather than by retrying"
                    .to_string(),
            ));
        }
        let Some(session) = last_session else {
            return Ok(Err("the stage has no previous session".to_string()));
        };
        // A session id belongs to the CLI that created it. Resumable only
        // when that CLI is still one this daemon has and is still the role's.
        if self.registry().lookup(None, &session.cli_adapter).is_err() {
            return Ok(Err(format!(
                "its session ran on cli '{}', which this daemon doesn't know (known CLIs: {}), \
                 so it can't be resumed",
                session.cli_adapter,
                self.registry().names().join(", ")
            )));
        }
        let current_cli = match self.current_role_cli(task, definition, stage_def) {
            Ok(cli) => cli,
            Err(why) => {
                return Ok(Err(format!("the role's CLI could not be resolved: {why}")));
            }
        };
        if current_cli != session.cli_adapter {
            return Ok(Err(format!(
                "the role's CLI changed from '{}' to '{current_cli}' since its session ran, and a \
                 session can only be resumed by the CLI that created it",
                session.cli_adapter
            )));
        }
        let end_reason = match session.end_reason {
            Some(
                reason @ (SessionEndReason::Interrupted
                | SessionEndReason::Reaped
                | SessionEndReason::DaemonStopped),
            ) => reason,
            Some(other) => {
                return Ok(Err(format!(
                    "its turn ended '{other}', which is the agent's own failure rather than an \
                     interruption"
                )));
            }
            None => {
                return Ok(Err(
                    "its process exited without saying why, so the session may be broken"
                        .to_string(),
                ));
            }
        };
        let Some(adapter_session_id) = session.adapter_session_id.clone() else {
            return Ok(Err("it never reported a session to resume".to_string()));
        };
        let resumes =
            sessions::resume_chain_len(&self.pool, &session.id, MAX_CONSECUTIVE_RESUMES).await?;
        if resumes >= MAX_CONSECUTIVE_RESUMES {
            return Ok(Err(format!(
                "its session has already been resumed {resumes} times in a row; starting over is \
                 the only way out of an interruption that keeps repeating"
            )));
        }
        Ok(Ok(ResumeSession {
            adapter_session_id,
            cli_adapter: session.cli_adapter.clone(),
            previous_session_id: session.id.clone(),
            end_reason,
        }))
    }

    /// The `cli` the stage's role resolves to right now, as a displayable
    /// error when it can't be resolved.
    fn current_role_cli(
        &self,
        task: &Task,
        definition: &WorkflowDefinition,
        stage_def: &StageDef,
    ) -> Result<String, String> {
        let StageKind::AgentTurn { role, .. } = &stage_def.kind else {
            return Err("the stage is not an agent turn".to_string());
        };
        let role_def = definition
            .roles
            .get(role)
            .ok_or_else(|| format!("unknown role '{role}'"))?;
        let global = self.load_global_config().map_err(|err| err.to_string())?;
        role_config::resolve_cli(role, role_def, &global, &task.config).map_err(|e| e.to_string())
    }

    /// Seeds `project_id`'s repo with the built-in workflows and their
    /// prompt files, under `<repo_path>/.chocofactory/workflows/` (issue
    /// #88: `choco project init-workflows <project>`) — an eject of the
    /// built-ins in this version of the daemon, via the same
    /// `config_root::seed_builtin_workflows` (so a repo-local copy has the
    /// exact same never-overwrite guarantee and creates-on-first-seed
    /// behaviour). Never touches git — no add, no commit; that's left to
    /// the operator, who `choco project init-workflows`'s own CLI output
    /// hints at.
    pub async fn init_project_workflows(
        &self,
        project_id: &str,
    ) -> Result<config_root::SeedReport, InitWorkflowsError> {
        let project = projects::get(&self.pool, project_id)
            .await?
            .ok_or_else(|| InitWorkflowsError::NoSuchProject(project_id.to_string()))?;
        let repo_path = project
            .repo_path
            .ok_or_else(|| InitWorkflowsError::NoRepoPath(project_id.to_string()))?;
        let repo = PathBuf::from(&repo_path);
        if !repo.is_dir() {
            return Err(InitWorkflowsError::RepoPathMissing(repo));
        }
        let workflows_dir = repo.join(".chocofactory").join("workflows");
        config_root::seed_builtin_workflows(&workflows_dir).map_err(InitWorkflowsError::Io)
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
                worktree::ensure(&repo, &project, &task.id, task.base_commit.as_deref())
                    .await
                    .map_err(WorkingDirError::Worktree)?;
                // `None` means the task row was deleted out from under this
                // call (same race `tasks::get` above is exposed to) —
                // surfaced the same way, not silently ignored.
                tasks::set_worktree(&self.pool, &task.id, &repo.to_string_lossy(), &project)
                    .await?
                    .ok_or(EngineError::NoSuchTask)?;
            }
            // `arrival` is seeded empty here (rather than left absent)
            // so the entry stage's `{{ arrival.from }}`/`{{ arrival.outcome }}`
            // render as "" with no `template_unresolved` note (#112) — the
            // same treatment `task` gets, and for the same reason: nothing
            // has transitioned yet.
            let mut payload = json!({
                "task": { "input": initial_input, "title": task.title },
                "arrival": { "from": "", "outcome": "" },
            });
            // An entry stage that is a `poll` gets its window in the same
            // INSERT as the row (#52).
            set_poll_window(&mut payload, definition, start, self.now())?;
            parallel::set_parallel_block(&mut payload, definition, start, self.now());
            let start_kind = definition
                .stages
                .get(start)
                .ok_or_else(|| EngineError::UnknownStage(start.to_string()))?
                .kind
                .name();
            let state =
                workflow_state::create(&self.pool, task_id, start, start_kind, payload).await?;
            self.enter_stage(
                task_id,
                definition,
                start,
                initial_input,
                None,
                &state.payload,
                None,
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
        self.advance_from_stage(task_id, definition, outcome, None, None, false)
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
        // `true` only on the reply path (`reply_to_gate`): stops the task's
        // detached runners — the gate's watcher — as the very last step
        // before the state write. A runner must never pass `true`: aborting
        // awaits every runner of the task, itself included.
        stop_watcher: bool,
    ) -> Result<(), EngineError> {
        let lock = self.lock_for_task(task_id).await;
        let _guard = lock.lock().await;

        // Set once `workflow_state::update` has committed the transition.
        let committed = std::sync::atomic::AtomicBool::new(false);

        // The stage entered on success, so the caller below can tell
        // whether it just became terminal without a second query.
        let result: Result<String, EngineError> = async {
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

            let (next_stage, transition) =
                self.compute_transition(definition, state, outcome, capture)?;

            // The reply path stops the gate's watcher here: under the
            // task lock, after every check that can refuse the
            // transition, and immediately before the write. Nothing
            // fallible may sit between this and `workflow_state::update`
            // — a refusal after the abort would leave the task open at
            // the gate with nothing watching it. If the update itself
            // fails, the caller marks the task stuck at the gate and
            // `retry` starts a new watcher.
            if stop_watcher {
                self.abort_detached_runners(task_id).await;
            }

            // The returned row is the authority on what was actually
            // committed, and it's what the next stage renders its
            // templates against (P2-3). A `None` here means the row
            // vanished between this function's read and its write —
            // impossible while the lock is held, but discarding the
            // `Option` would turn that broken invariant into a task that
            // silently transitions against state nothing persisted.
            let updated = workflow_state::update(&self.pool, task_id, transition)
                .await?
                .ok_or(EngineError::NoWorkflowState)?;
            committed.store(true, std::sync::atomic::Ordering::Relaxed);

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
                None,
            )
            .await?;
            Ok(next_stage)
        }
        .await;
        // A reply that fails before the state write for any reason other
        // than a benign race leaves its caller to mark the task stuck at
        // the gate. "Stuck" must mean nothing is running for that gate, so
        // stop the watcher here, still under the task lock: a watcher that
        // outlived the stuck mark could advance a stuck task, and a retry
        // would then start a second one. Benign races (`StageMovedOn`,
        // `UnknownOutcome`, cancelled) leave the task as it was, watcher
        // included.
        if stop_watcher
            && !committed.load(std::sync::atomic::Ordering::Relaxed)
            && let Err(err) = &result
            && !err.is_benign_resume_race()
        {
            self.abort_detached_runners(task_id).await;
        }
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

    /// The pure half of a transition: from `state`'s current stage, applies
    /// `outcome` through the stage's `on:` map (loop guard included), merges
    /// `capture`, stamps every engine-owned payload fact and returns the next
    /// stage with the one UPDATE that moves the task there. Shared by
    /// `advance_from_stage` and a parallel group's all-done settle, so the two
    /// can never write different things for "leaving a stage".
    pub(super) fn compute_transition(
        &self,
        definition: &Arc<WorkflowDefinition>,
        state: chocofactory_core::models::WorkflowState,
        outcome: &str,
        capture: Option<Value>,
    ) -> Result<(String, workflow_state::WorkflowStateUpdate), EngineError> {
        let from_stage = state.current_stage.clone();
        let stage_def = definition
            .stages
            .get(&from_stage)
            .ok_or_else(|| EngineError::UnknownStage(from_stage.clone()))?;

        if matches!(stage_def.kind, StageKind::Terminal) {
            return Err(EngineError::TerminalStageHasNoTransitions(from_stage));
        }

        let mut next_stage =
            stage_def
                .on
                .get(outcome)
                .cloned()
                .ok_or_else(|| EngineError::UnknownOutcome {
                    stage: from_stage.clone(),
                    outcome: outcome.to_string(),
                })?;

        let mut loop_counters = state.loop_counters;
        if let Some(guard) = &stage_def.loop_guard {
            if guard.on == outcome {
                let count = bump_loop_counter(&mut loop_counters, &from_stage);
                if count > u64::from(guard.max) {
                    next_stage = guard.then.clone();
                }
            } else {
                // Consecutive-count rule: any other outcome starts
                // the count over, in this transition's single write.
                reset_loop_counter(&mut loop_counters, &from_stage);
            }
        }
        clear_guards_escaping_to(&mut loop_counters, definition, &next_stage);

        let mut payload = state.payload;
        if let Some(value) = capture {
            // Keyed by the stage that produced it, which the caller has
            // confirmed is still the current one.
            merge_stage_capture(&mut payload, &from_stage, value);
        }
        // Records how the task arrived at `next_stage`, so
        // `coder-revise.md` and any other template can branch on
        // the actual transition (#112) instead of guessing from
        // which stale `stages.*` capture happens to be non-empty.
        // Written into this same payload/update so it commits
        // atomically with `current_stage` — no second write.
        set_arrival(&mut payload, &from_stage, outcome);
        // Marks `from_stage` as having finished a run, in the same
        // payload/update, so a later template can tell "never ran"
        // from "ran but stored no capture".
        mark_stage_finished(&mut payload, &from_stage);
        // Records when the task left `from_stage`, truncated to the
        // whole second (GitHub's timestamp format), in this same
        // payload/update. The review gate fences on it.
        set_left_at(
            &mut payload,
            &from_stage,
            &self.now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
        // Stamps (or clears) the poll window for `next_stage`, in
        // this same payload so the deadline commits in the one
        // UPDATE that moves `current_stage` (#52).
        set_poll_window(&mut payload, definition, &next_stage, self.now())?;
        // Seeds the next entry of a parallel group, or drops the finished
        // group's block, in the same UPDATE.
        parallel::set_parallel_block(&mut payload, definition, &next_stage, self.now());
        // Written in the same UPDATE as `current_stage`, so the two
        // can never disagree. Looked up after any loop-guard
        // redirect, and before the watcher is stopped: a missing
        // stage must fail while the watcher is still running.
        let stage_kind = definition
            .stages
            .get(&next_stage)
            .ok_or_else(|| EngineError::UnknownStage(next_stage.clone()))?
            .kind
            .name()
            .to_string();
        let update = workflow_state::WorkflowStateUpdate {
            current_stage: next_stage.clone(),
            stage_kind,
            loop_counters,
            payload,
            // Also true for an `on:` edge back to the same
            // stage: that is a new entry.
            enters_stage: true,
        };
        Ok((next_stage, update))
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
    #[allow(clippy::too_many_arguments)]
    async fn enter_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        input: Option<&str>,
        entered_via: Option<&str>,
        payload: &Value,
        // Set only by `retry_task_locked`, and only for an `agent_turn`
        // whose last run was interrupted from outside (#92): every other
        // way into a stage starts a session of its own.
        resume: Option<&ResumeSession>,
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
        match events::append_stage_transition(
            &self.pool,
            task_id,
            stage_name,
            entered_via,
            stage_def.kind.name(),
        )
        .await
        {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, %err,
                "failed to record stage transition event"
            ),
        }

        let entry = StageEntry {
            task_id,
            definition,
            stage_name,
            stage_def,
            payload,
            input,
            resume,
            branch: None,
        };
        let entered = self.dispatch_stage(&entry);
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
        // since rendering happens before any `session` exists.
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
    async fn dispatch_stage(self: &Arc<Self>, entry: &StageEntry<'_>) -> Result<(), EngineError> {
        match &entry.stage_def.kind {
            StageKind::AgentTurn { .. } => self.enter_agent_turn(entry).await,
            StageKind::Shell { .. } => self.enter_shell(entry).await,
            StageKind::Poll { .. } => self.enter_poll(entry).await,
            // Pauses the task for a person. Whatever relays the next human
            // message advances this stage — see `reply_to_gate`, which also
            // threads the message through as this stage's capture (#59) —
            // and a gate with a `watch:` also starts that watcher (#175).
            StageKind::HumanGate { .. } => self.enter_gate(entry).await,
            StageKind::Terminal => self.enter_terminal(entry).await,
            // Starts every branch; see `parallel.rs`.
            StageKind::Parallel { .. } => self.enter_group(entry).await,
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

/// Records the transition that just brought the task into its new current
/// stage, as an engine-owned `payload.arrival` sibling of `payload.stages`
/// (#112). `from_stage` is the stage being left and `outcome` is what it
/// reported — exactly the values `advance_from_stage` already has in scope,
/// so this is a plain write rather than anything computed from the events
/// timeline (that trail is best-effort and appended after this commits, and
/// it doesn't record the stage left — see `enter_stage`'s comment on
/// `append_stage_transition`).
///
/// Unlike `merge_stage_capture`, this always overwrites the whole
/// `arrival` value rather than merging into it: there is nothing under it
/// worth preserving between transitions.
fn set_arrival(payload: &mut Value, from_stage: &str, outcome: &str) {
    // Same non-object handling as `merge_stage_capture`, and for the same
    // reason: the engine owns this column, so a payload that isn't an
    // object should be replaced rather than silently dropping the write.
    if !payload.is_object() {
        *payload = json!({});
    }
    payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .insert(
            "arrival".to_string(),
            json!({ "from": from_stage, "outcome": outcome }),
        );
}

/// Adds `stage` to the engine-owned `payload.finished_stages` array, which
/// lists every stage that has finished a run (left through
/// `advance_from_stage`, whatever the outcome) once each, in first-finish
/// order. `template::render` uses it to tell a stage that hasn't run yet
/// (normal, not worth a `template_unresolved` event) from one that ran and
/// stored no capture (a real mismatch). Retry and the restart sweep
/// re-enter a stage without finishing it, so they never add to it.
fn mark_stage_finished(payload: &mut Value, stage: &str) {
    if !payload.is_object() {
        *payload = json!({});
    }
    let list = payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .entry(template::FINISHED_STAGES)
        .or_insert_with(|| json!([]));
    if !list.is_array() {
        *list = json!([]);
    }
    let list = list
        .as_array_mut()
        .expect("finished_stages was just ensured to be an array");
    if !list.iter().any(|s| s.as_str() == Some(stage)) {
        list.push(json!(stage));
    }
}

/// Records in the engine-owned `payload.left_at` object (stage name -> time)
/// when the task last left `stage` through `advance_from_stage`, whatever the
/// outcome. Overwrites that stage's earlier value and keeps other stages'.
/// Retry, rewatch and the restart sweep re-enter a stage without leaving it,
/// so they never write it. `template::render` exposes it as
/// `{{ left_at.<stage> }}`.
fn set_left_at(payload: &mut Value, stage: &str, at: &str) {
    if !payload.is_object() {
        *payload = json!({});
    }
    let map = payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .entry(template::LEFT_AT)
        .or_insert_with(|| json!({}));
    if !map.is_object() {
        *map = json!({});
    }
    map.as_object_mut()
        .expect("left_at was just ensured to be an object")
        .insert(stage.to_string(), json!(at));
}

/// Increments the guarded stage's transition count and rewrites its whole
/// `loop_counters` entry as `{ "count": n }`. A pre-#106 entry of the form
/// `{ "entered_from": …, "count": n }` keeps its count and loses
/// `entered_from` the first time it is bumped — the `entered_from` seeding
/// rationale from before #106 (matching a later reset-on-different-entry
/// check) no longer applies, since that check is gone.
fn bump_loop_counter(loop_counters: &mut Value, stage: &str) -> u64 {
    let obj = loop_counters
        .as_object_mut()
        .expect("engine always stores loop_counters as a JSON object");
    let count = obj
        .get(stage)
        .and_then(|entry| entry.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    obj.insert(stage.to_string(), json!({ "count": count }));
    count
}

/// Removes the guarded stage's `loop_counters` entry (no-op if absent; never
/// inserts a zero entry). Called when the stage resolves with an outcome
/// other than its guard's `on:`.
fn reset_loop_counter(loop_counters: &mut Value, stage: &str) {
    loop_counters
        .as_object_mut()
        .expect("engine always stores loop_counters as a JSON object")
        .remove(stage);
}

/// Clears the `loop_counters` entry of every stage whose `loop_guard.then`
/// is `next_stage` (#106): a guard's count starts over when the task
/// arrives at that guard's `then:` stage, whichever way it got there (the
/// guard's own reroute, or any other route: a failed command, a timeout,
/// another guard tripping). This is one of two resets; the other is the
/// guarded stage resolving with an outcome other than `on:`, in
/// `advance_from_stage` (see `reset_loop_counter`).
fn clear_guards_escaping_to(
    loop_counters: &mut Value,
    definition: &WorkflowDefinition,
    next_stage: &str,
) {
    let obj = loop_counters
        .as_object_mut()
        .expect("engine always stores loop_counters as a JSON object");
    for (stage, stage_def) in &definition.stages {
        if let Some(guard) = &stage_def.loop_guard
            && guard.then == next_stage
        {
            obj.remove(stage);
        }
    }
}

#[cfg(test)]
mod tests;
