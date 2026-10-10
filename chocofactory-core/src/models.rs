use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A grouping label for related tasks (design §3). May optionally carry a
/// repo path (issue #88): when set, a task created under this project
/// defaults its own `--repo`/`config.cwd` to it, and workflow resolution
/// looks in `<repo_path>/.chocofactory/workflows/` before the global
/// `~/.config/chocofactory/workflows/` directory — see `engine::
/// resolve_task_workflow`. `None` is today's behaviour: a project with no
/// repo of its own, every task under it configuring its own `--repo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// UUID (v4), stored as text. Kept as a plain `String` rather than a
    /// `Uuid` type so the generation scheme (e.g. a shorter id) can change
    /// later without touching every struct that carries an id.
    pub id: String,
    pub name: String,
    pub repo_path: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One unit of work, driven by a workflow definition (design §3, §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub workflow_def: String,
    pub title: String,
    /// Free-form for now (e.g. "open", "closed") — the full set of values
    /// is driven by workflow definitions, not fixed by this crate (§5.4).
    pub status: String,
    pub config: Value,
    /// The repo path and project name `worktree::ensure` actually used to
    /// create this task's worktree, snapshotted once at task start for a
    /// worktree-enabled workflow (§5.5 Q7, issue #58); `None` for every
    /// other task. Deliberately *not* derived from `config.cwd`/the
    /// project's current name on demand — both can change after the
    /// worktree already exists (`PATCH /tasks/{id}/config`, `PATCH
    /// /projects/{id}`), and re-deriving from their current values would
    /// let a later stage compute a path `ensure` never actually created.
    pub worktree_repo: Option<String>,
    pub worktree_project: Option<String>,
    /// Why this task is `status == "stuck"` (X-4, issue #61) — the engine
    /// gave up moving it forward on its own, e.g. a stage's outcome has no
    /// `on:` edge, a transition failed, or an agent turn's session never
    /// started. `None` for every other status: `update_status` clears it on
    /// any write, so it only ever holds a value alongside `"stuck"`, set by
    /// `db::tasks::mark_stuck`. `choco task retry <id>` re-runs the current
    /// stage and clears it via `db::tasks::reopen_stuck`.
    pub stuck_reason: Option<String>,
    /// `true` when the task was cancelled with `--keep` (#102): its agents
    /// were stopped but its worktree and branch were left in place for a
    /// person to take over. Written in the same `UPDATE` as the
    /// `cancelled` status, so the two can never disagree.
    #[serde(default)]
    pub kept_work: bool,
    /// The canonical, absolute path of the workflow file this task actually
    /// runs (issue #88) — resolved once at `create_task` time from either
    /// the project's own `.chocofactory/workflows/<workflow_def>.yaml` or
    /// the global `~/.config/chocofactory/workflows/<workflow_def>.yaml`
    /// (see `engine::resolve_task_workflow`). This, not `workflow_def`
    /// re-resolved by name, is *the* authority for which file every later
    /// reload of this task's workflow loads — `engine::WorkflowEngine::
    /// load_task_workflow` reads this path directly rather than searching
    /// again, so editing or deleting the global/repo copy after the task
    /// started can never change which file a running task uses. `None`
    /// only for a task created before this column existed, which falls
    /// back to a fresh name lookup exactly as it always has.
    ///
    /// A future iteration may repoint this at a different file mid-task
    /// (e.g. a task whose own stage generates a workflow for a later
    /// stage to run) — nothing here assumes it is fixed for the task's
    /// whole lifetime, only that it is always the single source of truth
    /// for "what runs next".
    pub workflow_path: Option<String>,
    /// SHA-256 of `workflow_path`'s contents (lowercase hex), hashed from
    /// the same read that parsed the file (`engine::load_workflow_file`) so
    /// the recorded digest can never describe a different file than the one
    /// that ran. Used only to detect drift for display (`choco task
    /// status`'s "changed since task start") — a workflow file changing
    /// after a task starts is allowed, not refused, and reloading never
    /// checks this hash. `None` alongside `workflow_path: None`.
    pub workflow_sha256: Option<String>,
    /// The ref the task's worktree was forked from, as resolved at create
    /// (`origin/main`, a `--base` value, or `HEAD`). Set once at create for a
    /// worktree workflow and never changed; `None` for older tasks and for
    /// workflows without a worktree.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// The full SHA `base_ref` resolved to at create. Same lifetime as
    /// `base_ref`.
    #[serde(default)]
    pub base_commit: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle state of an agent subprocess session (design §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Idle,
    Exited,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SessionStatus::Active => "active",
            SessionStatus::Idle => "idle",
            SessionStatus::Exited => "exited",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSessionStatusError(pub String);

impl fmt::Display for ParseSessionStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid session status: {}", self.0)
    }
}

impl std::error::Error for ParseSessionStatusError {}

impl FromStr for SessionStatus {
    type Err = ParseSessionStatusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(SessionStatus::Active),
            "idle" => Ok(SessionStatus::Idle),
            "exited" => Ok(SessionStatus::Exited),
            other => Err(ParseSessionStatusError(other.to_string())),
        }
    }
}

/// Why a `Session`'s `status` reached its current value, when `status`
/// alone is ambiguous — e.g. `Reaped` when a clean exit into `Idle` was
/// actually the idle reaper force-closing stdin, not the turn finishing on
/// its own. A proper enum (rather than bare string literals scattered
/// across `engine.rs`/`session.rs`) so a typo in one call site is a compile
/// error instead of a silently-broken comparison elsewhere (§ review on PR
/// #35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    /// The idle reaper force-closed stdin on a session past `idle_timeout`.
    Reaped,
    /// `SessionManager::start` failed to spawn the adapter process at all.
    StartFailed,
    /// An operator cancelled the task, and `SessionManager::cancel` killed
    /// this run's subprocess group (#69). Unlike every other variant this
    /// one is *requested* rather than observed, so it takes precedence over
    /// `Reaped` when both could apply — see `session::drain_session`.
    Cancelled,
    /// A single-shot turn had reported its outcome and ended (#90), but its
    /// process group was still alive once the post-completion grace period
    /// ran out, so `session::drain_session` killed it. Something the turn
    /// started kept running after it said it was done, so the task parks
    /// rather than advancing past work that may still be landing.
    Lingered,
    /// A single-shot turn ended without ever calling `report_outcome`
    /// (#90), even after the daemon's nudges, so it was closed rather than
    /// treated as complete.
    NoReport,
    /// The turn was cut off from outside rather than by anything the agent
    /// did (#92): the CLI reported a usage/rate limit for the account and
    /// ended the turn. The session's transcript is intact and the work was
    /// healthy, so `retry` resumes it instead of starting from scratch —
    /// unlike a crash, which would only be resumed straight back into
    /// itself. Detected by `adapter::claude`'s structured markers (see
    /// `AgentEvent::Interrupted`), never by the engine.
    Interrupted,
    /// The daemon stopped or restarted during the turn. Like `Cancelled`
    /// it is requested rather than observed (shutdown kills the process
    /// group, or the startup park sweep records it after a crash); like
    /// `Interrupted` the transcript is intact, so `retry` resumes it.
    DaemonStopped,
}

impl fmt::Display for SessionEndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SessionEndReason::Reaped => "reaped",
            SessionEndReason::StartFailed => "start_failed",
            SessionEndReason::Cancelled => "cancelled",
            SessionEndReason::Lingered => "lingered",
            SessionEndReason::NoReport => "no_report",
            SessionEndReason::Interrupted => "interrupted",
            SessionEndReason::DaemonStopped => "daemon_stopped",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSessionEndReasonError(pub String);

impl fmt::Display for ParseSessionEndReasonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid session end reason: {}", self.0)
    }
}

impl std::error::Error for ParseSessionEndReasonError {}

impl FromStr for SessionEndReason {
    type Err = ParseSessionEndReasonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "reaped" => Ok(SessionEndReason::Reaped),
            "start_failed" => Ok(SessionEndReason::StartFailed),
            "cancelled" => Ok(SessionEndReason::Cancelled),
            "lingered" => Ok(SessionEndReason::Lingered),
            "no_report" => Ok(SessionEndReason::NoReport),
            "interrupted" => Ok(SessionEndReason::Interrupted),
            "daemon_stopped" => Ok(SessionEndReason::DaemonStopped),
            other => Err(ParseSessionEndReasonError(other.to_string())),
        }
    }
}

/// One `open` task whose current stage a daemon restart would strand
/// (`GET /server`'s `in_flight`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    pub task_id: String,
    pub title: String,
    pub stage: String,
    /// `"agent_turn"`, `"shell"`, or `"unknown"` when the task's workflow
    /// could not be loaded or no longer has the stage.
    pub kind: String,
}

/// The body of `GET /server`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub version: String,
    pub commit: Option<String>,
    pub pid: u32,
    pub port: u16,
    pub started_at: DateTime<Utc>,
    pub config_root: String,
    pub exe: String,
    /// `None` when the startup stamp of the executable could not be taken.
    pub exe_replaced: Option<bool>,
    pub choco_binary: String,
    pub choco_binary_found: bool,
    pub tasks: std::collections::BTreeMap<String, i64>,
    pub in_flight: Vec<InFlight>,
}

/// What `choco task retry` should do with the agent session a stuck stage
/// left behind (#92). Shared between the CLI and the daemon so the request
/// body has one definition rather than a hand-built JSON object on one side
/// and a parser on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryMode {
    /// Resume the stage's interrupted session when it is safe to, and
    /// otherwise start a fresh one. The default: which of the two applies
    /// is something the daemon knows and the operator usually doesn't.
    #[default]
    Auto,
    /// Resume, or fail saying why it can't be resumed.
    Resume,
    /// Always start a fresh session, whatever ended the last one.
    Fresh,
}

/// What a retry did (#92) — the body of the daemon's `202`, so an operator
/// is told whether the stage picked up where it left off or started over,
/// rather than having to infer it from the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryOutcome {
    /// The stage that was re-entered.
    pub stage: String,
    /// Whether the stuck stage's agent session was resumed.
    pub resumed: bool,
    /// The CLI adapter's own identifier for the resumed session, when one
    /// was resumed — the value `--resume` was given, not a `sessions.id`.
    /// Named `adapter_session_id` rather than `session_id` so it can't be
    /// confused with the row identifier `Event.session_id` and
    /// `Session.id` mean elsewhere on the wire.
    pub adapter_session_id: Option<String>,
    /// Why the stage started fresh instead of resuming, when it did.
    /// `None` when it resumed. Carried rather than only logged, because
    /// "it started over" is the answer an operator is most likely to
    /// question, and the daemon is the only one holding the reason.
    pub fresh_reason: Option<String>,
    /// Whether the retry sent a task parked at a gate back to the watcher
    /// stage that timed out, instead of re-running a stuck stage.
    #[serde(default)]
    pub rewatched: bool,
}

/// One row per underlying agent subprocess session a task has had (§3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub task_id: String,
    pub stage: String,
    pub role: String,
    pub cli_adapter: String,
    pub model: String,
    /// The CLI adapter's own session identifier (written by
    /// `db::sessions::set_adapter_session_id`, used for `--resume`) — not
    /// to be confused with this row's own `id`.
    pub adapter_session_id: Option<String>,
    pub status: SessionStatus,
    pub end_reason: Option<SessionEndReason>,
    /// The session this one continued (#92), set when `retry` resumed an
    /// interrupted turn instead of starting a fresh session. `None` for a
    /// session that opened its own, which is every session that isn't a
    /// resume.
    pub resumed_from: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// Normalized event kind emitted by an agent adapter (design §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// A human-sent message — the initial prompt a task was created with, a
    /// `send_message` relay into an already-open `agent_turn` (P1-9), or a
    /// human's reply resuming a `human_gate` (#59). The first two are
    /// session-scoped (`session_id` set); a `human_gate` resume has no
    /// session to attribute it to, so that one is recorded task-scoped
    /// instead (`session_id` is `None`). This is the only variant here
    /// that isn't normalized from an agent adapter's own output; without
    /// it, `events` only ever recorded the agent's half of a conversation.
    HumanMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Thinking,
    Error,
    SessionMeta,
    /// The CLI's `result` line arrived: this turn is over and the process is
    /// now only waiting on stdin EOF to exit — it never exits on its own
    /// (#70). Always follows that turn's `AssistantMessage` row(s) in the
    /// timeline. Payload is `{"is_error"}` — the reply itself is already in
    /// `AssistantMessage`, and any `capture:` verdict lands separately in
    /// [`Self::TurnOutcome`].
    TurnCompleted,
    /// The task entered a workflow stage (X-3). Unlike every other variant
    /// this describes the *task*, not an agent session — `human_gate` and
    /// `terminal` stages never open one — so its `Event` has no
    /// `session_id`. Payload is `{"stage", "outcome"}`, where `outcome` is
    /// the transition that selected this stage and is null for the entry
    /// stage. Filtering a task's timeline for these replaces the former
    /// `workflow_state.stage_history` column.
    StageEntered,
    /// A `shell` stage's command ran to completion, was killed by its
    /// `timeout`, or failed to spawn at all (P2-1). Like [`Self::StageEntered`]
    /// this belongs to the *task* rather than to an agent session — a shell
    /// stage opens no session and has no `session` — so its `Event` has no
    /// `session_id`. Payload is `{"stage", "command", "exit_code",
    /// "timed_out", "duration_ms", "stdout_tail", "stderr_tail"}`, plus an
    /// optional `"note"` when something about the capture needs explaining
    /// (unparseable JSON, oversized output, a spawn failure). Without it a
    /// failed command would route through `on: error` with the reason
    /// nowhere in the API.
    ShellOutput,
    /// A capture-bearing `agent_turn` concluded (#45), or a turn on *any*
    /// stage called the `report_outcome` MCP tool (#73): what its `capture:`
    /// asked for, the outcome it transitioned on, and an optional `"note"`
    /// when those two didn't line up — a reply that wasn't valid JSON under
    /// `capture: json`, a reply or a report that carried no usable `outcome`
    /// and so fell back to `done`, or a report made on a stage that declares
    /// no `capture: json` and so doesn't route on it at all. Payload is
    /// `{"stage", "capture", "outcome", "applied", "note", "source"}`.
    ///
    /// `source` is `"tool"` when the outcome *actually came from* a
    /// `report_outcome` call, `"reply"` when it came from parsing the turn's
    /// final text (the fallback for an agent that never calls the tool, and
    /// also what a `capture: text` stage's outcome always comes from — see
    /// below), or `null` when neither applies: a non-capturing stage's turn
    /// (the common case, and today's only case before #73), whether or not
    /// a report happened to be made alongside it. A `capture: text` stage
    /// with a report is the same as no `capture:` with one — a report only
    /// routes a `capture: json` stage, so on any other stage `source`
    /// reflects where the *outcome actually taken* came from, and `"tool"`
    /// there would claim a report drove something it never touched. Either
    /// way the report itself isn't lost: `note` says one was made and this
    /// stage doesn't route on it.
    ///
    /// `applied` says whether the transition was actually taken: an outcome
    /// can be computed and then deliberately not applied, which is what a
    /// reviewer stage declaring no `done` edge relies on. For a report made
    /// on a stage without `capture: json`, `applied` reflects whatever that
    /// stage's normal (uncaptured) transition did — the report itself never
    /// drives routing there; only the `note` says the report was made and
    /// this stage doesn't route on it.
    ///
    /// Unlike [`Self::StageEntered`]/[`Self::ShellOutput`] this one *does*
    /// belong to a session — a turn has a `session` — so it carries a
    /// `session_id`. It exists so the lenient fallback above is visible in
    /// the timeline rather than only in the daemon's logs.
    ///
    /// Written after the transition it describes, so it sorts just *after*
    /// the `stage_entered` it explains, where [`Self::ShellOutput`] is
    /// written before its own and sorts before. The inconsistency is the
    /// price of `applied` being truthful (see `engine::finish_turn`).
    TurnOutcome,
    /// A `{{ stages.… }}`/`{{ task.… }}` placeholder in a stage's
    /// `command:`/`prompt_file` named a value that doesn't exist — a field
    /// the referenced stage's capture didn't carry this run, or a stage
    /// that finished without storing a capture (#60). A stage that simply
    /// hasn't finished a run yet is not recorded here. Rendered as an empty
    /// string rather than failing the stage — the loader can only check
    /// that the reference *parses* and names a capturing stage; whether a
    /// captured JSON payload actually carries the field is a run-time
    /// question. Like [`Self::StageEntered`]/[`Self::ShellOutput`] this
    /// belongs to the *task*, not a session — rendering happens before any
    /// `session` exists — so its `Event` has no `session_id`. Payload is
    /// `{"stage", "placeholders"}`, where `placeholders` lists every
    /// blanked-out reference from that one render (one event per render
    /// call, not per placeholder).
    TemplateUnresolved,
    /// A `shell`/`poll` stage's rendered `env:` value was over the size cap
    /// and was cut (#101). Task-scoped like [`Self::TemplateUnresolved`].
    /// Payload is `{"stage", "message", "env_truncated"}`.
    EnvTruncated,
    /// The engine deleted, or deliberately kept, a task's git branch when
    /// the task finished or was cancelled (#102). Task-scoped. Payload is
    /// `{"branch", "sha", "action", "message"}` plus `"reason"` (kept) or
    /// `"error"` (failed); `action` is `"deleting"`, `"kept"`
    /// or `"delete_failed"`. A `deleting` event is
    /// written before the branch is removed, so the tip is recoverable.
    BranchCleanup,
    /// The daemon itself acted on an agent session (#90). Session-scoped
    /// (`session_id` set). Payload is `{"kind", "message"}`, where `kind` is:
    /// - `"nudge"`: asked a turn that ended without calling `report_outcome`
    ///   to report;
    /// - `"no_report"`: closed a turn that never reported (nudges used up, or
    ///   its total job-wait limit ran out);
    /// - `"lingered"`: the agent process was still alive after its turn ended
    ///   and was killed;
    /// - `"job_wait"`: the turn ended without reporting while background jobs
    ///   run, so it is not nudged until they finish or its total job-wait
    ///   limit runs out;
    /// - `"background_jobs"`: the CLI's list of running background jobs
    ///   changed;
    /// - `"resume"`: a retry resumed the interrupted turn's conversation;
    /// - `"leftovers_killed"`: processes the turn started were still running
    ///   when it ended and were killed (survivors are listed separately);
    /// - `"leftovers_unchecked"`: the process table could not be read, so
    ///   what the turn left running could not be checked.
    ///
    /// Without it those interventions would only be visible in the daemon's
    /// logs and in the run's `end_reason`.
    SessionNote,
    /// The worktree state recorded when a `read_only` role's turn starts
    /// (#172). Session-scoped. Payload is `{"stage", "role", "cwd", "head",
    /// "branch", "status_sha256", "status_entries", "status",
    /// "inherited_from", "message"}`; `status` is the first 20 `git status`
    /// entries and `inherited_from` is the id of the resumed session whose
    /// baseline this turn reuses, or null for a fresh one.
    WorktreeBaseline,
    /// A `read_only` role's turn left the worktree different from its
    /// baseline (#172). Session-scoped. Payload is `{"stage", "role",
    /// "changes", "status_entries", "status", "message"}`; `changes` lists
    /// `{"field": "head"|"branch"|"status", "before", "after"}` and `message`
    /// is the reason the task was parked as stuck.
    WorktreeChanged,
    /// A parallel group started one of its branches. Task-scoped
    /// (`session_id` is `None`). Payload is `{"group", "branch", "kind",
    /// "entry", "via"}`: `group` is the group stage's name, `branch` the
    /// branch's name, `kind` its stage kind as YAML spells it, `entry` the
    /// group's entry number (an integer), and `via` is null for a start on
    /// entering the group, `"retry"` for a fresh start by a retry, or
    /// `"retry_resume"` for a retry that resumed the interrupted session.
    BranchStarted,
    /// A parallel group's branch finished. Task-scoped (`session_id` is
    /// `None`). Payload is `{"group", "branch", "entry", "state"}` plus
    /// `"result"` when `state` is `"done"` (the outcome the branch reported)
    /// or `"reason"` when it is `"failed"` (the failure text); never both.
    BranchFinished,
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EventType::HumanMessage => "human_message",
            EventType::AssistantMessage => "assistant_message",
            EventType::ToolCall => "tool_call",
            EventType::ToolResult => "tool_result",
            EventType::Thinking => "thinking",
            EventType::Error => "error",
            EventType::SessionMeta => "session_meta",
            EventType::TurnCompleted => "turn_completed",
            EventType::StageEntered => "stage_entered",
            EventType::ShellOutput => "shell_output",
            EventType::TurnOutcome => "turn_outcome",
            EventType::TemplateUnresolved => "template_unresolved",
            EventType::EnvTruncated => "env_truncated",
            EventType::BranchCleanup => "branch_cleanup",
            EventType::SessionNote => "session_note",
            EventType::WorktreeBaseline => "worktree_baseline",
            EventType::WorktreeChanged => "worktree_changed",
            EventType::BranchStarted => "branch_started",
            EventType::BranchFinished => "branch_finished",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEventTypeError(pub String);

impl fmt::Display for ParseEventTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid event type: {}", self.0)
    }
}

impl std::error::Error for ParseEventTypeError {}

impl FromStr for EventType {
    type Err = ParseEventTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "human_message" => Ok(EventType::HumanMessage),
            "assistant_message" => Ok(EventType::AssistantMessage),
            "tool_call" => Ok(EventType::ToolCall),
            "tool_result" => Ok(EventType::ToolResult),
            "thinking" => Ok(EventType::Thinking),
            "error" => Ok(EventType::Error),
            "session_meta" => Ok(EventType::SessionMeta),
            "turn_completed" => Ok(EventType::TurnCompleted),
            "stage_entered" => Ok(EventType::StageEntered),
            "shell_output" => Ok(EventType::ShellOutput),
            "turn_outcome" => Ok(EventType::TurnOutcome),
            "template_unresolved" => Ok(EventType::TemplateUnresolved),
            "env_truncated" => Ok(EventType::EnvTruncated),
            "branch_cleanup" => Ok(EventType::BranchCleanup),
            "session_note" => Ok(EventType::SessionNote),
            "worktree_baseline" => Ok(EventType::WorktreeBaseline),
            "worktree_changed" => Ok(EventType::WorktreeChanged),
            "branch_started" => Ok(EventType::BranchStarted),
            "branch_finished" => Ok(EventType::BranchFinished),
            other => Err(ParseEventTypeError(other.to_string())),
        }
    }
}

/// Append-only entry in a task's timeline (§3, §4.2).
///
/// Most entries are normalized from an agent session's output and name the
/// session they came from; `StageEntered`/`ShellOutput` entries belong to the
/// task itself and leave `session_id` `None`. Ordering across a task is always
/// `(created_at, id)` — there is no per-session sequence counter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub task_id: String,
    /// The agent session this entry came from, or `None` when it describes
    /// the task rather than a session (see [`EventType::StageEntered`]).
    pub session_id: Option<String>,
    pub event_type: EventType,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

/// Generic workflow-engine bookkeeping for a task, one row per task (§3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub task_id: String,
    pub current_stage: String,
    /// Kind of `current_stage` as workflow YAML spells it. `None` for a row
    /// from before the column that no transition, retry or startup sweep has
    /// reached yet.
    #[serde(default)]
    pub stage_kind: Option<String>,
    /// JSON object mapping stage name -> loop count (§5.3).
    pub loop_counters: Value,
    /// Stage-specific data (e.g. PR URL, last check status) owned by
    /// whichever stage kind is currently active.
    pub payload: Value,
    pub updated_at: DateTime<Utc>,
    /// When `current_stage` was last entered. `None` only for a row that
    /// predates the column and has no matching `stage_entered` event.
    pub stage_entered_at: Option<DateTime<Utc>>,
    /// The branches of the current parallel group, derived by the API.
    /// Always empty on a row read from the database; an empty array also
    /// means no group is current.
    #[serde(default)]
    pub branches: Vec<BranchStatus>,
}

/// One branch of the current parallel group, as `GET /tasks/{id}` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchStatus {
    pub name: String,
    /// The branch's kind as workflow YAML spells it; `None` when the
    /// workflow does not declare the branch (or could not be loaded).
    #[serde(default)]
    pub kind: Option<String>,
    /// `running`, `done`, `failed`, or `unknown`; kept a string so a state a
    /// later version adds still decodes.
    pub state: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// The group's current entry number (0 when unrecorded).
    #[serde(default)]
    pub entry: i64,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
}

/// How far the current parallel group has got, for the task list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchProgress {
    pub settled: u32,
    pub total: u32,
}

/// One row of `GET /tasks`: the task plus the few workflow facts a
/// dashboard needs, so it needs no per-task request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    #[serde(flatten)]
    pub task: Task,
    pub current_stage: Option<String>,
    pub stage_entered_at: Option<DateTime<Utc>>,
    /// `{}` when the task has no workflow_state row.
    pub loop_counters: Value,
    pub pr: Option<PullRequestRef>,
    /// The task is open and its current stage is a `human_gate` (#175).
    #[serde(default)]
    pub waiting_on_human: bool,
    /// What the task has cost so far; `None` when no turn has recorded
    /// usage (every task from before usage was recorded).
    #[serde(default)]
    pub usage_total: Option<UsageTotal>,
    /// Settled and total branches of the current parallel group; `None`
    /// when no group is current.
    #[serde(default)]
    pub branch_progress: Option<BranchProgress>,
}

/// A task's cost and token total as `GET /tasks` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageTotal {
    /// `None` when every turn's cost is unknown.
    pub cost_usd: Option<f64>,
    /// All four token kinds summed; `None` when none is known.
    pub tokens: Option<u64>,
    /// `"api_equivalent"` when every turn ran under a subscription login,
    /// else `"estimated"`.
    pub billing_label: String,
    /// Turns whose cost is unknown and so missing from `cost_usd`; when
    /// above 0 the cost is a lower bound.
    #[serde(default)]
    pub turns_without_cost: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub number: u64,
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_round_trips_through_display_and_from_str() {
        for status in [
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::Exited,
        ] {
            assert_eq!(status.to_string().parse::<SessionStatus>().unwrap(), status);
        }
    }

    #[test]
    fn session_status_from_str_rejects_unknown_value() {
        let err = "bogus".parse::<SessionStatus>().unwrap_err();
        assert_eq!(err.0, "bogus");
        assert_eq!(err.to_string(), "invalid session status: bogus");
    }

    #[test]
    fn session_end_reason_round_trips_through_display_and_from_str() {
        for reason in [
            SessionEndReason::Reaped,
            SessionEndReason::StartFailed,
            SessionEndReason::Cancelled,
            SessionEndReason::Lingered,
            SessionEndReason::NoReport,
            SessionEndReason::Interrupted,
            SessionEndReason::DaemonStopped,
        ] {
            assert_eq!(
                reason.to_string().parse::<SessionEndReason>().unwrap(),
                reason
            );
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{reason}\"")
            );
        }
    }

    #[test]
    fn session_end_reason_from_str_rejects_unknown_value() {
        let err = "bogus".parse::<SessionEndReason>().unwrap_err();
        assert_eq!(err.0, "bogus");
        assert_eq!(err.to_string(), "invalid session end reason: bogus");
    }

    #[test]
    fn event_type_round_trips_through_display_and_from_str() {
        for event_type in [
            EventType::HumanMessage,
            EventType::AssistantMessage,
            EventType::ToolCall,
            EventType::ToolResult,
            EventType::Thinking,
            EventType::Error,
            EventType::SessionMeta,
            EventType::TurnCompleted,
            EventType::StageEntered,
            EventType::ShellOutput,
            EventType::TurnOutcome,
            EventType::TemplateUnresolved,
            EventType::EnvTruncated,
            EventType::BranchCleanup,
            EventType::SessionNote,
            EventType::WorktreeBaseline,
            EventType::WorktreeChanged,
            EventType::BranchStarted,
            EventType::BranchFinished,
        ] {
            assert_eq!(
                event_type.to_string().parse::<EventType>().unwrap(),
                event_type
            );
        }
    }

    #[test]
    fn event_type_from_str_rejects_unknown_value() {
        let err = "bogus".parse::<EventType>().unwrap_err();
        assert_eq!(err.0, "bogus");
        assert_eq!(err.to_string(), "invalid event type: bogus");
    }

    #[test]
    fn session_status_serializes_to_snake_case_json() {
        assert_eq!(
            serde_json::to_string(&SessionStatus::Idle).unwrap(),
            "\"idle\""
        );
    }

    #[test]
    fn event_type_serializes_to_snake_case_json() {
        assert_eq!(
            serde_json::to_string(&EventType::ToolResult).unwrap(),
            "\"tool_result\""
        );
    }

    #[test]
    fn branch_events_serialize_and_round_trip() {
        assert_eq!(
            serde_json::to_string(&EventType::BranchStarted).unwrap(),
            "\"branch_started\""
        );
        for (event_type, name) in [
            (EventType::BranchStarted, "branch_started"),
            (EventType::BranchFinished, "branch_finished"),
        ] {
            let event = Event {
                id: "e".into(),
                task_id: "t".into(),
                session_id: None,
                event_type,
                payload: serde_json::json!({"group": "g"}),
                created_at: Utc::now(),
            };
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["event_type"], name);
            let back: Event = serde_json::from_str(&json.to_string()).unwrap();
            assert_eq!(back, event);
        }
    }
}
