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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chokofactory_core::models::{TaskRunEndReason, TaskRunStatus};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::adapter::RoleConfig;
use crate::db::{task_runs, tasks, workflow_state};
use crate::session::{SessionError, SessionManager};
use crate::workflow_def::{StageDef, StageKind, WorkflowDefinition};

/// How often the `agent_turn` completion watcher polls a `task_run`'s
/// status. Not configurable (yet) — this is an internal implementation
/// detail of auto-advancing single-shot turns, not a user-facing knob.
const TURN_WATCH_INTERVAL: Duration = Duration::from_millis(100);

pub struct WorkflowEngine {
    pool: SqlitePool,
    session_manager: Arc<SessionManager>,
    /// Serializes `advance()` calls per task (§ review on PR #35): without
    /// this, two callers racing to advance the same task's `workflow_state`
    /// (e.g. the turn-completion watcher and a future human_gate message
    /// relay, P1-9) could both read the same row and then both write,
    /// silently clobbering one call's `stage_history`/`loop_counters`.
    task_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
        }
    }
}

impl std::error::Error for EngineError {}

impl From<sqlx::Error> for EngineError {
    fn from(err: sqlx::Error) -> Self {
        EngineError::Db(err)
    }
}

impl WorkflowEngine {
    pub fn new(pool: SqlitePool, session_manager: Arc<SessionManager>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            session_manager,
            task_locks: Mutex::new(HashMap::new()),
        })
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
        workflow_state::create(&self.pool, task_id, start).await?;
        self.enter_stage(task_id, definition, start, initial_input)
            .await
    }

    /// Applies `outcome` against the task's current stage — looking it up
    /// in that stage's `on:` map and running any `loop_guard` (§5.3) —
    /// transitions `workflow_state`, and enters whatever stage results.
    ///
    /// Callers: the `agent_turn` completion watcher spawned by
    /// `enter_stage` (for a plain single-shot turn's `done`), and
    /// whatever receives a human's message during a `human_gate` (its
    /// `resumed`) — the latter not yet wired to an API layer (P1-9).
    pub async fn advance(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        outcome: &str,
    ) -> Result<(), EngineError> {
        let lock = self.lock_for_task(task_id).await;
        let _guard = lock.lock().await;

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
            .await
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
                // Best-effort, not `?`: `workflow_state` is already
                // committed to this terminal stage by the caller (`advance`/
                // `start_task`) before this runs, so a transient failure
                // here can't be un-done by returning early — it would only
                // additionally skip the lock eviction below and leak that
                // entry forever, since a terminal stage's own guard makes
                // this exact branch unreachable a second time for this task
                // (§ review on PR #35).
                if let Err(err) = tasks::update_status(&self.pool, task_id, "closed").await {
                    eprintln!(
                        "workflow engine: failed to mark task {task_id} closed after entering a terminal stage: {err}"
                    );
                }
                // A terminal task can never be `advance()`d or `start_task`'d
                // again, so its lock entry would otherwise sit in
                // `task_locks` forever — evict it now rather than growing
                // that map for the life of the daemon (§ review on PR #35).
                self.task_locks.lock().await.remove(task_id);
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
        let system_prompt = role_def
            .system_prompt_file
            .as_ref()
            .map(|path| fs::read_to_string(path).map_err(EngineError::Io))
            .transpose()?;

        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let cwd = task
            .config
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let role_config = RoleConfig {
            cwd,
            model: Some(role_def.model.clone()),
            system_prompt,
        };

        let task_run = task_runs::create(
            &self.pool,
            task_runs::NewTaskRun {
                task_id,
                stage: stage_name,
                role,
                cli_adapter: &role_def.cli,
                model: &role_def.model,
            },
        )
        .await?;

        if let Err(err) = self
            .session_manager
            .start(&task_run.id, &prompt, &role_config)
            .await
        {
            // The task_run row was just created `Active` above; without
            // this, a spawn failure here leaves it Active forever (nothing
            // else in this module ever transitions it), wedging the task
            // since workflow_state was already committed to this stage by
            // the caller before enter_stage ran (§ review on PR #35).
            //
            // This still leaves the *task* itself — as opposed to this
            // task_run — with no queryable "stuck" signal beyond this
            // eprintln! and `end_reason: "start_failed"` on the task_run:
            // nothing here marks `workflow_state`/`tasks` in a way an
            // operator or API layer could discover without already knowing
            // to look. Acknowledged gap for Phase 1; surfacing it (e.g. a
            // task status or a query joining `tasks` to a stalled
            // `task_run`) is expected to land with the API layer (P1-9) or
            // a dedicated follow-up, not silently absorbed here.
            if let Err(update_err) = task_runs::update_status(
                &self.pool,
                &task_run.id,
                TaskRunStatus::Exited,
                Some(Utc::now()),
                Some(TaskRunEndReason::StartFailed),
            )
            .await
            {
                eprintln!(
                    "workflow engine: failed to mark task run {} exited after a failed session start: {update_err}",
                    task_run.id
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
                        eprintln!(
                            "workflow engine: task run {task_run_id} (task {task_id}) was force-closed by the idle reaper before completing its turn; not auto-advancing"
                        );
                        return;
                    }
                    Ok(Some(run)) if run.status == TaskRunStatus::Idle => break,
                    Ok(Some(run)) if run.status == TaskRunStatus::Exited => {
                        eprintln!(
                            "workflow engine: task run {task_run_id} (task {task_id}) exited without completing its turn cleanly; not auto-advancing"
                        );
                        return;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        eprintln!(
                            "workflow engine: task run {task_run_id} (task {task_id}) disappeared while watching for turn completion; not auto-advancing"
                        );
                        return;
                    }
                    Err(err) => {
                        eprintln!(
                            "workflow engine: failed to poll task run {task_run_id} (task {task_id}) while watching for turn completion: {err}; not auto-advancing"
                        );
                        return;
                    }
                }
                tokio::time::sleep(TURN_WATCH_INTERVAL).await;
            }
            if let Err(err) = engine.advance(&task_id, &definition, "done").await {
                eprintln!(
                    "workflow engine: failed to auto-advance task {task_id} on turn completion: {err}"
                );
            }
        });
    }
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
        let session_manager =
            SessionManager::new(pool.clone(), adapter, chrono::Duration::hours(1));
        WorkflowEngine::new(pool, session_manager)
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
