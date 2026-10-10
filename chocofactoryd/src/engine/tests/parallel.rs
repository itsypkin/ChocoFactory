//! Running a parallel group (#257 PG1-4): enter, run branches, settle.

use super::*;
use crate::engine::parallel::{BranchApplied, BranchOutcome, BranchWatch, set_parallel_block};
use chocofactory_core::models::{SessionEndReason, WorkflowState};

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Summarize,
    /// An agent_turn target with no `prompt_file`, so entering it fails.
    Bare,
    Back,
}

#[derive(Clone, Copy)]
struct Shape {
    /// Reached from a shell stage, else the entry stage.
    prep: bool,
    one_prompt: bool,
    two_prompt: bool,
    /// Branches `capture: json` with `results: [clean, blocking]`.
    json: bool,
    target: Target,
}

impl Shape {
    fn new() -> Self {
        Shape {
            prep: true,
            one_prompt: true,
            two_prompt: true,
            json: true,
            target: Target::Summarize,
        }
    }
}

fn yaml(shape: Shape) -> String {
    let branch = |name: &str, prompt: bool| {
        let mut parts = vec!["kind: agent_turn".to_string(), "role: reviewer".to_string()];
        if prompt {
            parts.push(format!("prompt_file: {name}.md"));
        }
        if shape.json {
            parts.push("capture: json".to_string());
            parts.push("results: [clean, blocking]".to_string());
        }
        format!("      {name}: {{ {} }}\n", parts.join(", "))
    };
    let (done_target, extra) = match shape.target {
        Target::Summarize => (
            "summarize",
            "  summarize:\n    kind: agent_turn\n    role: reviewer\n    prompt_file: summarize.md\n    on: { done: finished }\n",
        ),
        Target::Bare => (
            "summarize",
            "  summarize:\n    kind: agent_turn\n    role: reviewer\n    on: { done: finished }\n",
        ),
        Target::Back => (
            "back",
            "  back:\n    kind: shell\n    command: \"true\"\n    loop_guard: { on: done, max: 1, then: finished }\n    on: { done: panel }\n",
        ),
    };
    let prep = if shape.prep {
        "  prep:\n    kind: shell\n    command: \"true\"\n    on: { done: panel }\n"
    } else {
        ""
    };
    format!(
        "name: group-flow\nworktree: true\nroles:\n  reviewer:\n    cli: claude\n    model: opus\n    \
         read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n{prep}  panel:\n    \
         kind: parallel\n    branches:\n{}{}    on: {{ done: {done_target} }}\n{extra}  finished:\n    \
         kind: terminal\n",
        branch("one", shape.one_prompt),
        branch("two", shape.two_prompt),
    )
}

struct Group {
    pool: SqlitePool,
    engine: Arc<WorkflowEngine>,
    task_id: String,
    def: Arc<WorkflowDefinition>,
    _dirs: Vec<TempDir>,
}

/// A task on `shape`'s workflow, not yet started. `script` is the fake
/// claude's script for every session.
async fn group(shape: Shape, script: Value) -> Group {
    group_with(shape, script, |binary, pool, dir| {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
        let events_notify = Arc::new(Notify::new());
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
            fast_turn_timers(),
        );
        WorkflowEngine::new(pool, manager, dir.to_path_buf(), None, events_notify)
    })
    .await
}

async fn group_with(
    shape: Shape,
    script: Value,
    build: impl FnOnce(&str, SqlitePool, &Path) -> Arc<WorkflowEngine>,
) -> Group {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let repo = tempdir();
    init_git_repo(&repo).await;
    fs::write(dir.join("one.md"), "BRANCH-ONE review").unwrap();
    fs::write(dir.join("two.md"), "BRANCH-TWO review").unwrap();
    fs::write(
        dir.join("summarize.md"),
        "SUMMARIZE {{ stages.one.summary }} and {{ stages.two.summary }}",
    )
    .unwrap();
    let text = yaml(shape);
    fs::write(dir.join("group-flow.yaml"), &text).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&text, &dir).unwrap());
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = tasks::create(
        &pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: &def.name,
            title: "T",
            config: json!({ "cwd": repo.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
            base_ref: None,
            base_commit: None,
        },
    )
    .await
    .unwrap()
    .id;
    let binary = named_script_binary(&dir, "fake-claude-group", script);
    let engine = build(&binary, pool.clone(), &dir);
    Group {
        pool,
        engine,
        task_id,
        def,
        _dirs: vec![dir, repo],
    }
}

impl Group {
    async fn start(&self, input: Option<&str>) -> Result<(), EngineError> {
        self.engine
            .start_task(&self.task_id, &self.def, input)
            .await
    }

    async fn state(&self) -> WorkflowState {
        workflow_state::get(&self.pool, &self.task_id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn status(&self) -> String {
        tasks::get(&self.pool, &self.task_id)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn sessions(&self, stage: &str) -> Vec<Session> {
        runs_for_stage(&self.pool, &self.task_id, stage).await
    }

    /// The session facts of `stage`, with their laps.
    async fn facts(&self, stage: &str) -> Vec<crate::usage::SessionFacts> {
        crate::db::usage::list_session_facts(&self.pool, &self.task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|f| f.stage == stage)
            .collect()
    }

    /// Waits for `n` sessions of `stage`.
    async fn wait_sessions(&self, stage: &str, n: usize) -> Vec<Session> {
        crate::test_support::wait_until(&format!("{n} session(s) of {stage}"), || async {
            let found = self.sessions(stage).await;
            if found.len() >= n {
                Ok(found)
            } else {
                Err(format!("{} session(s)", found.len()))
            }
        })
        .await
    }

    async fn wait_status(&self, status: &str) {
        wait_until_task_status(&self.pool, &self.task_id, status).await
    }

    /// The payload once every branch has left `running`.
    async fn settled_payload(&self) -> Value {
        crate::test_support::wait_until("no running branch", || async {
            let payload = self.state().await.payload;
            let running = payload
                .pointer("/parallel/branches")
                .and_then(Value::as_object)
                .is_some_and(|b| b.values().any(|s| s["state"] == "running"));
            if running {
                Err(payload.to_string())
            } else {
                Ok(payload)
            }
        })
        .await
    }

    async fn events_of(&self, kind: EventType) -> Vec<chocofactory_core::models::Event> {
        events::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == kind)
            .collect()
    }

    async fn finish(
        &self,
        entry: i64,
        branch: &str,
        session_id: &str,
        end: BranchOutcome,
    ) -> BranchApplied {
        self.engine
            .finish_branch(
                &self.task_id,
                &self.def,
                &BranchWatch {
                    group: "panel".to_string(),
                    entry,
                },
                branch,
                session_id,
                end,
            )
            .await
    }
}

fn set_script(g: &Group, script: Value) {
    fs::write(
        g._dirs[0].join("fake-claude-group.json"),
        script.to_string(),
    )
    .unwrap();
}

async fn retry(g: &Group, mode: RetryMode) -> Result<RetryOutcome, RetryTaskError> {
    g.engine.retry_task(&g.task_id, mode).await
}

/// Everything a refused retry must leave alone.
async fn snapshot(g: &Group) -> (String, Option<String>, Value, usize, usize) {
    let task = tasks::get(&g.pool, &g.task_id).await.unwrap().unwrap();
    (
        task.status,
        task.stuck_reason,
        g.state().await.payload,
        sessions::list_for_task(&g.pool, &g.task_id)
            .await
            .unwrap()
            .len(),
        events::list_for_task(&g.pool, &g.task_id)
            .await
            .unwrap()
            .len(),
    )
}

async fn started_events(g: &Group, branch: &str) -> Vec<chocofactory_core::models::Event> {
    g.events_of(EventType::BranchStarted)
        .await
        .into_iter()
        .filter(|e| e.payload["branch"] == branch)
        .collect()
}

async fn wait_settle_errors(g: &Group, n: usize) {
    crate::test_support::wait_until("settle errors", || async {
        let found = g
            .events_of(EventType::Error)
            .await
            .iter()
            .filter(|e| e.payload["stuck"] == true)
            .count();
        if found >= n {
            Ok(())
        } else {
            Err(format!("{found} settle error(s)"))
        }
    })
    .await;
}

/// How many transition and branch entries the timeline holds.
async fn timeline_len(g: &Group) -> usize {
    let mut n = 0;
    for kind in [
        EventType::StageEntered,
        EventType::BranchStarted,
        EventType::BranchFinished,
        EventType::Error,
    ] {
        n += g.events_of(kind).await.len();
    }
    n
}

fn reported(result: &str, summary: &str) -> BranchOutcome {
    BranchOutcome::Done {
        result: result.to_string(),
        capture: Some(json!({ "outcome": result, "summary": summary })),
        note: None,
    }
}

fn report_steps(outcome: &str, summary: &str) -> Value {
    json!([
        {"op": "read_turn"},
        {"op": "report", "outcome": outcome, "summary": summary},
        {"op": "result"},
    ])
}

fn hold_steps() -> Value {
    json!([{"op": "read_turn"}, {"op": "sleep", "seconds": 8}])
}

fn summarize_steps() -> Value {
    json!([{"op": "echo_turn"}, {"op": "sleep", "seconds": 8}])
}

fn by_prompt(cases: &[(&str, Value)]) -> Value {
    let map: serde_json::Map<String, Value> = cases
        .iter()
        .map(|(marker, steps)| (marker.to_string(), steps.clone()))
        .collect();
    json!({ "by_prompt": map })
}

/// Both branches report `outcome`; the summary target holds.
fn both_report(outcome: &str) -> Value {
    by_prompt(&[
        ("BRANCH-ONE", report_steps(outcome, "sum-one")),
        ("BRANCH-TWO", report_steps(outcome, "sum-two")),
        ("SUMMARIZE", summarize_steps()),
    ])
}

fn holding() -> Value {
    by_prompt(&[
        ("BRANCH-ONE", hold_steps()),
        ("BRANCH-TWO", hold_steps()),
        ("SUMMARIZE", summarize_steps()),
    ])
}

fn branch_slot<'a>(payload: &'a Value, branch: &str) -> &'a Value {
    &payload["parallel"]["branches"][branch]
}

// 1
#[tokio::test]
async fn all_branches_done_moves_the_task_to_the_done_target() {
    let g = group(Shape::new(), both_report("clean")).await;
    g.start(None).await.unwrap();
    wait_until_stage(&g.pool, &g.task_id, "summarize").await;
    let target = g.wait_sessions("summarize", 1).await.remove(0);
    // The target's prompt carries both branch captures: the loader accepted
    // `stages.<branch>.summary` and the engine rendered it.
    crate::test_support::wait_until("both summaries in the target prompt", || async {
        let events = events::list_for_session(&g.pool, &target.id).await.unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
            .collect();
        if text.contains("sum-one") && text.contains("sum-two") {
            Ok(())
        } else {
            Err(text)
        }
    })
    .await;
    let state = g.state().await;
    assert_eq!(state.current_stage, "summarize");
    assert!(state.payload.get("parallel").is_none(), "{}", state.payload);
    let finished = state.payload["finished_stages"].as_array().unwrap().clone();
    for stage in ["one", "two", "panel"] {
        assert!(finished.contains(&json!(stage)), "{finished:?}");
    }
    assert_eq!(
        state.payload["arrival"],
        json!({"from": "panel", "outcome": "done"})
    );
    assert_eq!(state.payload["stages"]["one"]["summary"], "sum-one");
    let finished_events = g.events_of(EventType::BranchFinished).await;
    assert_eq!(finished_events.len(), 2);
    assert!(finished_events.iter().all(|e| e.payload["state"] == "done"));
}

// 2
#[tokio::test]
async fn a_result_outside_results_fails_the_branch() {
    let script = by_prompt(&[
        ("BRANCH-ONE", report_steps("bogus", "x")),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("'one'"), "{reason}");
    assert!(reason.contains("'bogus'"), "{reason}");
    assert!(reason.contains("[clean, blocking]"), "{reason}");
    assert!(reason.contains("1 of 2 branch(es) failed"), "{reason}");
    let state = g.state().await;
    let payload = &state.payload;
    assert_eq!(branch_slot(payload, "one")["state"], "failed");
    assert!(payload["stages"].get("one").is_none(), "capture merged");
    assert_eq!(branch_slot(payload, "two")["state"], "done");
    assert_eq!(payload["stages"]["two"]["summary"], "sum-two");
    assert_eq!(state.current_stage, "panel");
}

// 3 and 13 (no report is not resumable)
#[tokio::test]
async fn a_failed_branch_settles_with_its_sibling_kept() {
    let script = by_prompt(&[
        ("BRANCH-ONE", json!([{"op": "read_turn"}, {"op": "result"}])),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("'one'"), "{reason}");
    assert!(
        reason.contains("without calling report_outcome"),
        "{reason}"
    );
    let payload = g.state().await.payload;
    assert_eq!(branch_slot(&payload, "two")["state"], "done");
    assert_eq!(payload["stages"]["two"]["summary"], "sum-two");
    assert_eq!(branch_slot(&payload, "one")["state"], "failed");
    assert_eq!(branch_slot(&payload, "one")["resumable"], false);
    assert!(branch_slot(&payload, "one")["ended_at"].is_string());
    // The Error event of the settle names the group.
    let errors = g.events_of(EventType::Error).await;
    assert!(
        errors
            .iter()
            .any(|e| e.payload["stage"] == "panel" && e.payload["stuck"] == true),
        "{errors:?}"
    );
    // The branch's own failure is on the timeline before the settle's error.
    let all = events::list_for_task(&g.pool, &g.task_id).await.unwrap();
    let at = |kind: EventType, stage_key: &str, name: &str| {
        all.iter()
            .position(|e| e.event_type == kind && e.payload[stage_key] == name)
            .unwrap_or_else(|| panic!("no {kind:?} for {name}"))
    };
    assert!(
        at(EventType::BranchFinished, "branch", "one") < at(EventType::Error, "stage", "panel")
    );
}

// 13
#[tokio::test]
async fn a_usage_limit_interruption_is_resumable() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
        ),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("usage limit"), "{reason}");
    let payload = g.state().await.payload;
    assert_eq!(branch_slot(&payload, "one")["state"], "failed");
    assert_eq!(branch_slot(&payload, "one")["resumable"], true);
}

// 4
#[tokio::test]
async fn a_stale_entry_or_session_writes_nothing() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    g.wait_sessions("two", 1).await;
    let before = g.state().await.payload;
    let events_before = timeline_len(&g).await;

    let old_entry = g.finish(0, "one", &one.id, reported("clean", "x")).await;
    assert!(
        matches!(old_entry, BranchApplied::Dropped(_)),
        "{old_entry:?}"
    );
    let other_session = g
        .finish(1, "one", "not-the-session", reported("clean", "x"))
        .await;
    assert!(
        matches!(other_session, BranchApplied::Dropped(_)),
        "{other_session:?}"
    );

    assert_eq!(g.state().await.payload, before);
    assert_eq!(g.status().await, "open");
    assert_eq!(timeline_len(&g).await, events_before);
}

// 5 and 7 (laps on entries 1 and 2)
#[tokio::test]
async fn re_entering_the_group_runs_every_branch_again() {
    let shape = Shape {
        target: Target::Back,
        ..Shape::new()
    };
    let g = group(shape, both_report("clean")).await;
    g.start(None).await.unwrap();
    g.wait_status("closed").await;
    let state = g.state().await;
    assert_eq!(state.payload["parallel_entries"]["panel"], 2);
    assert!(state.payload.get("parallel").is_none());
    for branch in ["one", "two"] {
        let mut laps: Vec<_> = g.facts(branch).await.into_iter().map(|f| f.lap).collect();
        laps.sort();
        assert_eq!(laps, [Some(1), Some(2)], "{branch}");
    }
    let started = g.events_of(EventType::BranchStarted).await;
    for entry in [1, 2] {
        for branch in ["one", "two"] {
            assert_eq!(
                started
                    .iter()
                    .filter(|e| e.payload["branch"] == branch && e.payload["entry"] == entry)
                    .count(),
                1,
                "{branch} entry {entry}: {started:?}"
            );
        }
    }
}

// 6
#[tokio::test]
async fn cancelling_mid_group_leaves_the_payload_alone() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    let before = g.state().await.payload;
    g.engine.cancel_task(&g.task_id, false).await.unwrap();
    crate::test_support::wait_until("both branch sessions cancelled", || async {
        let mut all = g.sessions("one").await;
        all.extend(g.sessions("two").await);
        if all.len() == 2
            && all
                .iter()
                .all(|s| s.end_reason == Some(SessionEndReason::Cancelled))
        {
            Ok(())
        } else {
            Err(format!("{all:?}"))
        }
    })
    .await;
    // Give the watchers (100ms poll) time to see it and, wrongly, write.
    tokio::time::sleep(StdDuration::from_millis(600)).await;
    assert_eq!(g.state().await.payload, before);
    assert_eq!(g.status().await, "cancelled");
}

// 8
#[tokio::test]
async fn usage_has_one_lap_line_per_branch() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    let facts = crate::db::usage::list_session_facts(&g.pool, &g.task_id)
        .await
        .unwrap();
    let rows: Vec<_> = facts
        .iter()
        .map(|f| crate::usage::UsageRow {
            session_id: f.id.clone(),
            billing: "api".to_string(),
            cost_usd: Some(0.01),
            tokens: crate::adapter::TokenCounts {
                input: Some(1),
                output: Some(1),
                cache_read: None,
                cache_write: None,
            },
            models: None,
        })
        .collect();
    let now = chrono::Utc::now();
    let usage = crate::usage::aggregate(
        crate::usage::TaskTimes {
            status: "open",
            created_at: now,
            updated_at: now,
        },
        &facts,
        &rows,
        &[],
        now,
    )
    .unwrap();
    let mut laps: Vec<_> = usage
        .by_lap
        .iter()
        .map(|l| (l.stage.as_str(), l.lap))
        .collect();
    laps.sort();
    assert_eq!(laps, [("one", Some(1)), ("two", Some(1))]);
}

// 9 and 10 (R1)
#[tokio::test]
async fn a_start_failure_does_not_stop_the_sibling_and_the_starts_are_recorded_first() {
    // The group is the entry stage, so on entry 1 the task's input is every
    // prompt-less branch's prompt; after the `back` transition there is none,
    // so `one` (declared first, no prompt_file) fails to start on entry 2.
    let shape = Shape {
        prep: false,
        one_prompt: false,
        target: Target::Back,
        ..Shape::new()
    };
    let g = group(shape, both_report("clean")).await;
    g.start(Some("BRANCH-ONE kickoff")).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("'one'"), "{reason}");
    assert!(reason.contains("1 of 2 branch(es) failed"), "{reason}");

    let payload = g.settled_payload().await;
    assert_eq!(payload["parallel"]["entry"], 2);
    assert_eq!(branch_slot(&payload, "one")["state"], "failed");
    assert_eq!(branch_slot(&payload, "one")["resumable"], false);
    assert_eq!(branch_slot(&payload, "two")["state"], "done");

    // Entry 2's surviving branch got lap 2, which only counts correctly if
    // every entry-2 `branch_started` existed before its session row.
    let two = g.facts("two").await;
    assert_eq!(two.len(), 2);
    let second = two.iter().find(|s| s.lap == Some(2)).expect("lap 2");
    assert_eq!(
        g.sessions("one").await.len(),
        1,
        "one never started on entry 2"
    );

    let started = g.events_of(EventType::BranchStarted).await;
    for entry in [1, 2] {
        for branch in ["one", "two"] {
            let found: Vec<_> = started
                .iter()
                .filter(|e| e.payload["branch"] == branch && e.payload["entry"] == entry)
                .collect();
            assert_eq!(found.len(), 1, "{branch} entry {entry}");
            assert!(found[0].payload["via"].is_null());
        }
    }
    let entry_two: Vec<_> = started.iter().filter(|e| e.payload["entry"] == 2).collect();
    assert!(
        entry_two.iter().all(|e| e.created_at <= second.started_at),
        "entry-2 starts must precede the first entry-2 session"
    );
    // And on the timeline.
    let timeline = events::list_for_task(&g.pool, &g.task_id).await.unwrap();
    let position = |id: &str| timeline.iter().position(|e| e.id == id).unwrap();
    let first_session_event = timeline
        .iter()
        .position(|e| e.session_id.as_deref() == Some(second.id.as_str()))
        .unwrap();
    assert!(
        entry_two
            .iter()
            .all(|e| position(&e.id) < first_session_event)
    );
}

// 12 and 16
#[tokio::test]
async fn every_branch_failing_to_start_parks_the_group_and_retry_re_enters_both() {
    let shape = Shape {
        one_prompt: false,
        two_prompt: false,
        ..Shape::new()
    };
    let g = group(shape, holding()).await;
    // The group is reached from `prep`, so the branches get no input.
    g.start(None).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("2 of 2 branch(es) failed"), "{reason}");
    assert!(reason.contains("'one'"), "{reason}");
    assert!(reason.contains("'two'"), "{reason}");
    assert!(reason.contains("no input"), "{reason}");
    assert!(g.sessions("one").await.is_empty());
    assert!(g.sessions("two").await.is_empty());
    let all: Vec<_> = sessions::list_for_task(&g.pool, &g.task_id).await.unwrap();
    assert!(
        all.iter().all(|s| s.status != SessionStatus::Active),
        "{all:?}"
    );

    let outcome = g
        .engine
        .retry_task(&g.task_id, RetryMode::Auto)
        .await
        .unwrap();
    assert_eq!(outcome.stage, "panel");
    assert!(!outcome.resumed);
    assert_eq!(outcome.branches.len(), 2);
    for b in &outcome.branches {
        assert!(!b.resumed);
        assert_eq!(
            b.fresh_reason.as_deref(),
            Some("the stage has no previous session")
        );
    }
    for branch in ["one", "two"] {
        let retried = g
            .events_of(EventType::BranchStarted)
            .await
            .into_iter()
            .filter(|e| e.payload["branch"] == branch && e.payload["via"] == "retry")
            .count();
        assert_eq!(retried, 1, "{branch}");
    }
    // They fail to start again (no input), and the task is stuck again.
    let again = stuck_reason(&g.pool, &g.task_id).await;
    assert!(again.contains("2 of 2 branch(es) failed"), "{again}");
    assert!(again.contains("no input"), "{again}");
    assert!(
        sessions::list_for_task(&g.pool, &g.task_id)
            .await
            .unwrap()
            .is_empty()
    );
}

// 12: the entry stage can fail every branch too, and enter_group returns Ok.
#[tokio::test]
async fn a_group_whose_branches_all_fail_to_start_returns_ok_from_the_start() {
    let shape = Shape {
        prep: false,
        one_prompt: false,
        two_prompt: false,
        ..Shape::new()
    };
    let g = group(shape, holding()).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
}

// 11
#[tokio::test]
async fn simultaneous_finishes_settle_exactly_once() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    let two = g.wait_sessions("two", 1).await.remove(0);
    let (a, b) = tokio::join!(
        g.finish(1, "one", &one.id, reported("clean", "sum-one")),
        g.finish(1, "two", &two.id, reported("clean", "sum-two")),
    );
    assert_eq!(a, BranchApplied::Recorded { done: true });
    assert_eq!(b, BranchApplied::Recorded { done: true });
    g.wait_sessions("summarize", 1).await;
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert_eq!(g.sessions("summarize").await.len(), 1);
    let entered: Vec<_> = g
        .events_of(EventType::StageEntered)
        .await
        .into_iter()
        .filter(|e| e.payload["stage"] == "summarize")
        .collect();
    assert_eq!(entered.len(), 1);
    let payload = g.state().await.payload;
    assert_eq!(payload["stages"]["one"]["summary"], "sum-one");
    assert_eq!(payload["stages"]["two"]["summary"], "sum-two");
}

// 14
#[tokio::test]
async fn a_read_only_violation_names_the_branches_that_ran_beside() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([
                {"op": "read_turn"},
                {"op": "run", "command": "touch violation.txt"},
                {"op": "report", "outcome": "clean", "summary": "x"},
                {"op": "result"},
            ]),
        ),
        ("BRANCH-TWO", hold_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    crate::test_support::wait_until("branch one failed", || async {
        let payload = g.state().await.payload;
        let slot = branch_slot(&payload, "one").clone();
        if slot["state"] == "failed" {
            Ok(slot)
        } else {
            Err(slot.to_string())
        }
    })
    .await;
    let payload = g.state().await.payload;
    let reason = branch_slot(&payload, "one")["reason"].as_str().unwrap();
    assert!(reason.contains("changed the worktree"), "{reason}");
    assert!(reason.ends_with("; ran beside: two"), "{reason}");
    // Its sibling is still running, so the task is not stuck yet.
    assert_eq!(g.status().await, "open");
}

// 15
#[tokio::test]
async fn a_group_as_the_entry_stage_runs_its_branches() {
    let shape = Shape {
        prep: false,
        ..Shape::new()
    };
    let g = group(shape, holding()).await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    let state = g.state().await;
    assert_eq!(state.current_stage, "panel");
    assert_eq!(state.stage_kind.as_deref(), Some("parallel"));
    assert_eq!(state.payload["parallel_entries"]["panel"], 1);
    assert_eq!(state.payload["parallel"]["entry"], 1);
    assert_eq!(branch_slot(&state.payload, "one")["state"], "running");
    assert_eq!(g.status().await, "open");
    assert_eq!(g.facts("one").await[0].lap, Some(1));
    assert_eq!(g.facts("two").await[0].lap, Some(1));
}

// 17: a branch's allowed outcomes are its results, and its turn is
// single-shot (the watcher exists, so a report finishes it — test 1).
struct OutcomeAdapter {
    inner: crate::adapter::ClaudeAdapter,
    seen: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

impl crate::adapter::AgentAdapter for OutcomeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn start(
        &self,
        prompt: &str,
        cfg: &crate::adapter::RoleConfig,
    ) -> Result<crate::adapter::AgentHandle, crate::adapter::AdapterError> {
        self.seen
            .lock()
            .unwrap()
            .push((prompt.to_string(), cfg.report_outcomes.clone()));
        self.inner.start(prompt, cfg)
    }

    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &crate::adapter::RoleConfig,
    ) -> Result<crate::adapter::AgentHandle, crate::adapter::AdapterError> {
        self.inner.resume(session_id, prompt, cfg)
    }
}

#[tokio::test]
async fn a_branch_may_report_its_results_or_done() {
    let adapter_slot: Arc<std::sync::Mutex<Option<Arc<OutcomeAdapter>>>> = Default::default();
    let slot = Arc::clone(&adapter_slot);
    let shape = Shape {
        prep: false,
        ..Shape::new()
    };
    // `two` has no capture: build it from a json shape and strip one.
    let g = group_with(shape, holding(), move |binary, pool, _dir| {
        let adapter = Arc::new(OutcomeAdapter {
            inner: crate::adapter::ClaudeAdapter::with_binary(binary),
            seen: Default::default(),
        });
        *slot.lock().unwrap() = Some(Arc::clone(&adapter));
        let events_notify = Arc::new(Notify::new());
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
        );
        WorkflowEngine::new(pool, manager, PathBuf::from("."), None, events_notify)
    })
    .await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    let adapter = adapter_slot.lock().unwrap().clone().unwrap();
    let seen = adapter.seen.lock().unwrap().clone();
    for (prompt, outcomes) in seen {
        // Both branches have `capture: json` in this shape.
        assert!(prompt.starts_with("BRANCH-"), "{prompt}");
        assert_eq!(outcomes, ["clean", "blocking"]);
    }
}

#[tokio::test]
async fn a_branch_without_capture_may_only_report_done() {
    let adapter_slot: Arc<std::sync::Mutex<Option<Arc<OutcomeAdapter>>>> = Default::default();
    let slot = Arc::clone(&adapter_slot);
    let shape = Shape {
        prep: false,
        json: false,
        // The default target templates branch captures, which a branch with
        // no `capture:` does not have.
        target: Target::Back,
        ..Shape::new()
    };
    let g = group_with(shape, holding(), move |binary, pool, _dir| {
        let adapter = Arc::new(OutcomeAdapter {
            inner: crate::adapter::ClaudeAdapter::with_binary(binary),
            seen: Default::default(),
        });
        *slot.lock().unwrap() = Some(Arc::clone(&adapter));
        let events_notify = Arc::new(Notify::new());
        let manager = SessionManager::new(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
        );
        WorkflowEngine::new(pool, manager, PathBuf::from("."), None, events_notify)
    })
    .await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    let adapter = adapter_slot.lock().unwrap().clone().unwrap();
    let seen = adapter.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|(_, outcomes)| outcomes == &["done"]));
}

// 18
#[tokio::test]
async fn a_finish_on_a_task_that_is_not_open_writes_nothing() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    g.wait_sessions("two", 1).await;
    assert!(
        tasks::mark_stuck(&g.pool, &g.task_id, "manual")
            .await
            .unwrap()
    );
    let before = g.state().await.payload;
    let applied = g.finish(1, "one", &one.id, reported("clean", "x")).await;
    assert!(matches!(applied, BranchApplied::Dropped(_)), "{applied:?}");
    let after = g.state().await.payload;
    assert_eq!(after, before);
    assert_eq!(branch_slot(&after, "one")["state"], "running");
}

// 19
#[tokio::test]
async fn entering_a_group_without_its_block_fails_and_writes_nothing() {
    let g = group(Shape::new(), holding()).await;
    let empty = json!({});
    let entry = StageEntry {
        task_id: &g.task_id,
        definition: &g.def,
        stage_name: "panel",
        stage_def: &g.def.stages["panel"],
        payload: &empty,
        input: None,
        resume: None,
        branch: None,
    };
    let err = g.engine.enter_group(&entry).await.unwrap_err();
    assert!(
        matches!(&err, EngineError::GroupStateMissing { stage } if stage == "panel"),
        "{err}"
    );
    assert!(g.events_of(EventType::BranchStarted).await.is_empty());
    assert!(
        sessions::list_for_task(&g.pool, &g.task_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn set_parallel_block_counts_entries_and_removes_the_block_elsewhere() {
    let dir = tempdir();
    for name in ["one.md", "two.md", "summarize.md"] {
        fs::write(dir.join(name), "x").unwrap();
    }
    let def = WorkflowDefinition::parse(&yaml(Shape::new()), &dir).unwrap();
    let now = chrono::Utc::now();
    let mut payload = json!({});
    set_parallel_block(&mut payload, &def, "panel", now);
    assert_eq!(payload["parallel_entries"]["panel"], 1);
    assert_eq!(payload["parallel"]["entry"], 1);
    assert_eq!(payload["parallel"]["branches"]["one"]["state"], "running");
    assert!(payload["parallel"]["branches"]["two"]["started_at"].is_string());
    set_parallel_block(&mut payload, &def, "panel", now);
    assert_eq!(payload["parallel"]["entry"], 2);
    assert_eq!(payload["parallel_entries"]["panel"], 2);
    set_parallel_block(&mut payload, &def, "summarize", now);
    assert!(payload.get("parallel").is_none());
    assert_eq!(payload["parallel_entries"]["panel"], 2);
}

#[tokio::test]
async fn a_branch_that_already_ended_or_a_group_already_left_is_dropped() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    let two = g.wait_sessions("two", 1).await.remove(0);

    let first = g
        .finish(1, "one", &one.id, reported("clean", "sum-one"))
        .await;
    assert_eq!(first, BranchApplied::Recorded { done: true });
    let after_first = g.state().await.payload;
    // `one` is no longer running: a second end for it changes nothing.
    let again = g
        .finish(1, "one", &one.id, reported("blocking", "late"))
        .await;
    assert!(matches!(again, BranchApplied::Dropped(_)), "{again:?}");
    assert_eq!(g.state().await.payload, after_first);

    let last = g
        .finish(1, "two", &two.id, reported("clean", "sum-two"))
        .await;
    assert_eq!(last, BranchApplied::Recorded { done: true });
    g.wait_sessions("summarize", 1).await;
    // The task has left the group: a late end for the settled entry is dropped.
    let late = g
        .finish(1, "two", &two.id, reported("clean", "again"))
        .await;
    assert!(matches!(late, BranchApplied::Dropped(_)), "{late:?}");
    assert_eq!(g.state().await.current_stage, "summarize");
}

#[tokio::test]
async fn a_failed_branch_start_record_starts_nothing_and_parks_the_group() {
    let g = group(Shape::new(), holding()).await;
    // Fails the second branch's insert: the first must be rolled back too.
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_branch_two BEFORE INSERT ON events
         WHEN NEW.event_type = 'branch_started' AND json_extract(NEW.payload, '$.branch') = 'two'
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    g.start(None).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("2 of 2 branch(es) failed"), "{reason}");
    assert!(
        reason.contains("could not record the branch start"),
        "{reason}"
    );
    assert!(g.events_of(EventType::BranchStarted).await.is_empty());
    assert!(
        sessions::list_for_task(&g.pool, &g.task_id)
            .await
            .unwrap()
            .is_empty()
    );
    let payload = g.state().await.payload;
    assert_eq!(branch_slot(&payload, "one")["state"], "failed");
    assert_eq!(branch_slot(&payload, "one")["resumable"], false);
}

#[tokio::test]
async fn a_failed_settle_writes_neither_the_payload_nor_the_stuck_status() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    let two = g.wait_sessions("two", 1).await.remove(0);
    let first = g
        .finish(1, "two", &two.id, reported("clean", "sum-two"))
        .await;
    assert_eq!(first, BranchApplied::Recorded { done: true });
    let before = g.state().await.payload;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_stuck BEFORE UPDATE ON tasks
         WHEN NEW.status = 'stuck'
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let applied = g
        .finish(
            1,
            "one",
            &one.id,
            BranchOutcome::Failed {
                reason: "it broke".to_string(),
            },
        )
        .await;
    assert!(matches!(applied, BranchApplied::Parked(_)), "{applied:?}");
    // The transaction rolled back: the branch is still running in the
    // payload, and the task is still open.
    assert_eq!(g.state().await.payload, before);
    assert_eq!(g.status().await, "open");
}

// A done target that cannot be entered parks the task there.
#[tokio::test]
async fn a_done_target_that_cannot_be_entered_parks_the_task() {
    let mut shape = Shape::new();
    shape.target = Target::Bare;
    let g = group(shape, both_report("clean")).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(
        reason.contains("stage 'summarize': could not be entered after 'panel' completed"),
        "{reason}"
    );
    let state = g.state().await;
    assert_eq!(state.current_stage, "summarize");
    assert!(state.payload.get("parallel").is_none());
}

// A database failure while recording a branch's end parks the task.
#[tokio::test]
async fn a_failed_branch_write_parks_the_task_with_the_error() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    g.wait_sessions("two", 1).await;
    let before = g.state().await.payload;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_state_update BEFORE UPDATE ON workflow_state
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let applied = g.finish(1, "one", &one.id, reported("clean", "x")).await;
    assert!(matches!(applied, BranchApplied::Parked(_)), "{applied:?}");
    assert_eq!(g.status().await, "stuck");
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(
        reason.contains("could not record the branch's end"),
        "{reason}"
    );
    assert_eq!(g.state().await.payload, before);
}

// The timeline's `turn_outcome` says what became of each branch's turn.
#[tokio::test]
async fn turn_outcome_events_say_whether_a_branch_was_applied() {
    let script = by_prompt(&[
        ("BRANCH-ONE", report_steps("bogus", "x")),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let events = g.events_of(EventType::TurnOutcome).await;
    let of = |stage: &str| {
        events
            .iter()
            .find(|e| e.payload["stage"] == stage)
            .unwrap_or_else(|| panic!("no turn_outcome for {stage}: {events:?}"))
            .payload
            .clone()
    };
    let one = of("one");
    assert_eq!(one["applied"], false, "{one}");
    assert_eq!(one["note"], "branch recorded as failed", "{one}");
    let two = of("two");
    assert_eq!(two["applied"], true, "{two}");
}

#[tokio::test]
async fn a_turn_outcome_for_a_dropped_branch_end_is_not_applied() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([
                {"op": "read_turn"},
                {"op": "sleep", "seconds": 2},
                {"op": "report", "outcome": "clean", "summary": "x"},
                {"op": "result"},
            ]),
        ),
        ("BRANCH-TWO", hold_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    assert!(
        tasks::mark_stuck(&g.pool, &g.task_id, "manual")
            .await
            .unwrap()
    );
    let event = crate::test_support::wait_until("one's turn_outcome", || async {
        g.events_of(EventType::TurnOutcome)
            .await
            .into_iter()
            .find(|e| e.payload["stage"] == "one")
            .ok_or_else(|| "none yet".to_string())
    })
    .await;
    assert_eq!(event.payload["applied"], false, "{event:?}");
    let note = event.payload["note"].as_str().unwrap_or_default();
    assert!(note.starts_with("not applied:"), "{event:?}");
    assert_eq!(
        branch_slot(&g.state().await.payload, "one")["state"],
        "running"
    );
}

// A reply that cannot be read back fails the branch.
#[tokio::test]
async fn an_unreadable_reply_fails_the_branch() {
    let script = by_prompt(&[
        ("BRANCH-ONE", hold_steps()),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    // A malformed event on one's session makes the query that reads its
    // reply back fail.
    sqlx::query(
        "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
         VALUES ('bad-event', ?, ?, 'assistant_message', 'not json', datetime('now'))",
    )
    .bind(&g.task_id)
    .bind(&one.id)
    .execute(&g.pool)
    .await
    .unwrap();
    // The turn ends idle with no report, so the watcher goes to read it back.
    sessions::update_status(&g.pool, &one.id, SessionStatus::Idle, None, None)
        .await
        .unwrap();
    g.wait_status("stuck").await;
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(
        reason.contains("the turn's reply could not be read back"),
        "{reason}"
    );
    let payload = g.state().await.payload;
    assert_eq!(branch_slot(&payload, "one")["state"], "failed");
}

// ---- retrying a group (PG1-5) ----

fn one_fails_two_clean() -> Value {
    by_prompt(&[
        ("BRANCH-ONE", report_steps("bogus", "x")),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
        ("SUMMARIZE", summarize_steps()),
    ])
}

#[tokio::test]
async fn retry_re_runs_only_the_failed_branch() {
    let script = by_prompt(&[
        ("BRANCH-ONE", report_steps("clean", "sum-one")),
        ("BRANCH-TWO", report_steps("bogus", "x")),
        ("SUMMARIZE", summarize_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let before = g.state().await.payload;
    set_script(
        &g,
        by_prompt(&[
            ("BRANCH-ONE", report_steps("clean", "sum-one")),
            ("BRANCH-TWO", report_steps("clean", "sum-two")),
            ("SUMMARIZE", summarize_steps()),
        ]),
    );
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    assert_eq!(outcome.stage, "panel");
    assert!(!outcome.resumed);
    assert_eq!(outcome.adapter_session_id, None);
    assert!(!outcome.rewatched);
    assert_eq!(outcome.branches.len(), 1);
    assert_eq!(outcome.branches[0].branch, "two");
    assert!(!outcome.branches[0].resumed);
    assert!(outcome.branches[0].fresh_reason.is_some());
    let top = outcome.fresh_reason.unwrap();
    assert!(
        top.starts_with("1 of 1 re-run branch(es) start fresh: 'two' ("),
        "{top}"
    );

    let target = g.wait_sessions("summarize", 1).await.remove(0);
    crate::test_support::wait_until("both summaries", || async {
        let events = events::list_for_session(&g.pool, &target.id).await.unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
            .collect();
        if text.contains("sum-one") && text.contains("sum-two") {
            Ok(())
        } else {
            Err(text)
        }
    })
    .await;
    assert_eq!(g.sessions("two").await.len(), 2);
    assert_eq!(g.sessions("one").await.len(), 1);
    // The ordinary retry writes no extra plain start: the retried session
    // keeps the group's entry as its lap.
    for branch in ["one", "two"] {
        let plain = started_events(&g, branch)
            .await
            .iter()
            .filter(|e| e.payload["via"].is_null())
            .count();
        assert_eq!(plain, 1, "{branch}");
    }
    let laps: Vec<_> = g.facts("two").await.into_iter().map(|f| f.lap).collect();
    assert_eq!(laps, [Some(1), Some(1)]);
    let state = g.state().await;
    assert_eq!(state.payload["stages"]["one"], before["stages"]["one"]);
    assert_eq!(state.current_stage, "summarize");
}

#[tokio::test]
async fn retry_resumes_a_usage_limited_branch() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
        ),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
        ("SUMMARIZE", summarize_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let old = g.sessions("one").await.remove(0);
    set_script(
        &g,
        by_prompt(&[
            (
                "Your previous turn on this stage was interrupted",
                report_steps("clean", "sum-one"),
            ),
            ("BRANCH-TWO", report_steps("clean", "sum-two")),
            ("SUMMARIZE", summarize_steps()),
        ]),
    );
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    assert!(outcome.resumed);
    assert_eq!(outcome.fresh_reason, None);
    assert_eq!(outcome.adapter_session_id, None);
    assert_eq!(
        outcome.branches,
        vec![chocofactory_core::models::RetriedBranch {
            branch: "one".to_string(),
            resumed: true,
            adapter_session_id: Some(old.adapter_session_id.clone().unwrap()),
            fresh_reason: None,
        }]
    );
    let starts = started_events(&g, "one").await;
    assert!(starts.iter().any(|e| e.payload["via"] == "retry_resume"));
    let sessions = g.wait_sessions("one", 2).await;
    let new = sessions.iter().find(|s| s.id != old.id).unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
    g.wait_sessions("summarize", 1).await;
}

#[tokio::test]
async fn a_mixed_retry_reports_resumed_false_and_names_only_the_fresh_branch() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
        ),
        ("BRANCH-TWO", report_steps("bogus", "x")),
        ("SUMMARIZE", summarize_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let old = g.sessions("one").await.remove(0);
    set_script(
        &g,
        by_prompt(&[
            (
                "Your previous turn on this stage was interrupted",
                report_steps("clean", "sum-one"),
            ),
            ("BRANCH-TWO", report_steps("clean", "sum-two")),
            ("SUMMARIZE", summarize_steps()),
        ]),
    );
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    assert!(!outcome.resumed);
    let top = outcome.fresh_reason.clone().unwrap();
    assert!(
        top.starts_with("1 of 2 re-run branch(es) start fresh: 'two' ("),
        "{top}"
    );
    assert!(!top.contains("'one'"), "{top}");
    assert_eq!(outcome.branches.len(), 2);
    assert_eq!(
        outcome.branches[0],
        chocofactory_core::models::RetriedBranch {
            branch: "one".to_string(),
            resumed: true,
            adapter_session_id: Some(old.adapter_session_id.clone().unwrap()),
            fresh_reason: None,
        }
    );
    assert_eq!(outcome.branches[1].branch, "two");
    assert!(!outcome.branches[1].resumed);
    assert!(outcome.branches[1].fresh_reason.is_some());
}

#[tokio::test]
async fn resume_with_a_non_resumable_branch_changes_nothing() {
    let script = by_prompt(&[
        ("BRANCH-ONE", report_steps("bogus", "x")),
        (
            "BRANCH-TWO",
            json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
        ),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Resume).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotResumable(s) if s.contains("branch 'one'") && s.contains("parallel stage 'panel'")),
        "{err}"
    );
    assert_eq!(snapshot(&g).await, before);
}

#[tokio::test]
async fn a_fast_second_failure_still_parks() {
    let g = group(Shape::new(), one_fails_two_clean()).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    wait_settle_errors(&g, 1).await;
    retry(&g, RetryMode::Auto).await.unwrap();
    wait_settle_errors(&g, 2).await;
    g.wait_status("stuck").await;
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("'one'"), "{reason}");
    assert_eq!(g.sessions("one").await.len(), 2);
}

async fn install_no_state_update(g: &Group) {
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_state_update BEFORE UPDATE ON workflow_state
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn retry_is_refused_while_a_branch_is_live_then_re_enters_dead_running_slots() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    g.wait_sessions("two", 1).await;
    install_no_state_update(&g).await;
    let applied = g.finish(1, "one", &one.id, reported("clean", "x")).await;
    assert!(matches!(applied, BranchApplied::Parked(_)), "{applied:?}");
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::BranchStillActive { stage, branch } if stage == "panel" && branch == "one"),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "branch 'one' of parallel stage 'panel' still has a live session; wait for it to end, then retry (or cancel the task)"
    );
    assert_eq!(snapshot(&g).await, before);

    // End the live sessions deliberately instead of racing a timer.
    sqlx::query("UPDATE sessions SET status = 'exited' WHERE task_id = ?")
        .bind(&g.task_id)
        .execute(&g.pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe("DROP TRIGGER no_state_update"))
        .execute(&g.pool)
        .await
        .unwrap();
    set_script(&g, both_report("clean"));
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    assert_eq!(outcome.branches.len(), 2);
    for branch in ["one", "two"] {
        g.wait_sessions(branch, 2).await;
        assert!(
            started_events(&g, branch)
                .await
                .iter()
                .any(|e| e.payload["via"] == "retry"),
            "{branch}"
        );
    }
}

async fn entry_two_start_failed(extra: bool) -> Group {
    let shape = Shape {
        target: Target::Back,
        ..Shape::new()
    };
    let g = group(shape, both_report("clean")).await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_entry_two BEFORE INSERT ON events
         WHEN NEW.event_type = 'branch_started' AND json_extract(NEW.payload, '$.entry') = 2
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    sqlx::query(sqlx::AssertSqlSafe("DROP TRIGGER no_entry_two"))
        .execute(&g.pool)
        .await
        .unwrap();
    if extra {
        sqlx::query(
            "UPDATE sessions SET end_reason = 'interrupted' WHERE task_id = ? AND stage = 'one'",
        )
        .bind(&g.task_id)
        .execute(&g.pool)
        .await
        .unwrap();
    }
    g
}

#[tokio::test]
async fn retry_after_failed_entry_starts_gives_entry_laps() {
    let g = entry_two_start_failed(false).await;
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    for b in &outcome.branches {
        assert_eq!(
            b.fresh_reason.as_deref(),
            Some("its last session belongs to an earlier entry of parallel stage 'panel'")
        );
    }
    for branch in ["one", "two"] {
        g.wait_sessions(branch, 2).await;
        let mut laps: Vec<_> = g.facts(branch).await.into_iter().map(|f| f.lap).collect();
        laps.sort();
        assert_eq!(laps, [Some(1), Some(2)], "{branch}");
        let two: Vec<_> = started_events(&g, branch)
            .await
            .into_iter()
            .filter(|e| e.payload["entry"] == 2)
            .collect();
        assert_eq!(two.len(), 2, "{two:?}");
        assert!(two[0].payload["via"].is_null());
        assert_eq!(two[1].payload["via"], "retry");
    }
}

#[tokio::test]
async fn an_earlier_entry_session_is_never_resumed() {
    let g = entry_two_start_failed(true).await;
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Resume).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotResumable(s) if s.contains("branch 'one'") && s.contains("earlier entry")),
        "{err}"
    );
    assert_eq!(snapshot(&g).await, before);
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    let one = outcome.branches.iter().find(|b| b.branch == "one").unwrap();
    assert!(!one.resumed);
    assert!(one.fresh_reason.as_ref().unwrap().contains("earlier entry"));
}

#[tokio::test]
async fn a_late_end_from_an_earlier_entry_writes_nothing() {
    let shape = Shape {
        target: Target::Back,
        ..Shape::new()
    };
    let g = group(shape, holding()).await;
    g.start(None).await.unwrap();
    let one = g.wait_sessions("one", 1).await.remove(0);
    let two = g.wait_sessions("two", 1).await.remove(0);
    assert_eq!(
        g.finish(1, "one", &one.id, reported("clean", "a")).await,
        BranchApplied::Recorded { done: true }
    );
    assert_eq!(
        g.finish(1, "two", &two.id, reported("clean", "b")).await,
        BranchApplied::Recorded { done: true }
    );
    let sessions = g.wait_sessions("one", 2).await;
    g.wait_sessions("two", 2).await;
    let new_one = sessions.iter().find(|s| s.id != one.id).unwrap();
    assert_eq!(g.state().await.payload["parallel"]["entry"], 2);
    let before = (
        g.state().await.payload,
        g.status().await,
        timeline_len(&g).await,
    );
    let applied = g
        .finish(1, "one", &new_one.id, reported("clean", "x"))
        .await;
    assert!(matches!(applied, BranchApplied::Dropped(_)), "{applied:?}");
    assert_eq!(
        (
            g.state().await.payload,
            g.status().await,
            timeline_len(&g).await
        ),
        before
    );
}

#[tokio::test]
async fn fresh_starts_a_resumable_branch_over() {
    let script = by_prompt(&[
        (
            "BRANCH-ONE",
            json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
        ),
        ("BRANCH-TWO", report_steps("clean", "sum-two")),
        ("SUMMARIZE", summarize_steps()),
    ]);
    let g = group(Shape::new(), script).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let old = g.sessions("one").await.remove(0);
    set_script(&g, both_report("clean"));
    let outcome = retry(&g, RetryMode::Fresh).await.unwrap();
    assert!(!outcome.resumed);
    assert_eq!(
        outcome.branches[0].fresh_reason.as_deref(),
        Some("a fresh start was asked for")
    );
    assert!(
        started_events(&g, "one")
            .await
            .iter()
            .any(|e| e.payload["via"] == "retry")
    );
    let sessions = g.wait_sessions("one", 2).await;
    let new = sessions.iter().find(|s| s.id != old.id).unwrap();
    assert_eq!(new.resumed_from, None);
}

#[tokio::test]
async fn a_failed_starts_transaction_during_retry_parks_the_group() {
    let g = group(Shape::new(), one_fails_two_clean()).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_retry_start BEFORE INSERT ON events
         WHEN NEW.event_type = 'branch_started' AND json_extract(NEW.payload, '$.via') = 'retry'
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let sessions_before = g.sessions("one").await.len();
    retry(&g, RetryMode::Auto).await.unwrap();
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(
        reason.contains("could not record the branch start"),
        "{reason}"
    );
    assert_eq!(g.sessions("one").await.len(), sessions_before);
}

#[tokio::test]
async fn retry_refuses_a_missing_block_and_a_group_with_nothing_failed() {
    let g = group(Shape::new(), one_fails_two_clean()).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    let state = g.state().await;
    let write = |payload: Value| {
        workflow_state::update(
            &g.pool,
            &g.task_id,
            workflow_state::WorkflowStateUpdate {
                current_stage: state.current_stage.clone(),
                stage_kind: "parallel".to_string(),
                loop_counters: state.loop_counters.clone(),
                payload,
                enters_stage: false,
            },
        )
    };

    let mut all_done = state.payload.clone();
    for branch in ["one", "two"] {
        all_done["parallel"]["branches"][branch] = json!({"state": "done"});
    }
    write(all_done).await.unwrap();
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NothingToRetry { stage } if stage == "panel"),
        "{err}"
    );
    assert_eq!(snapshot(&g).await, before);

    let mut missing = state.payload.clone();
    missing.as_object_mut().unwrap().remove("parallel");
    write(missing).await.unwrap();
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::GroupStateMissing { stage } if stage == "panel"),
        "{err}"
    );
    assert_eq!(snapshot(&g).await, before);
}

/// The no-input group: every start fails, so the task parks with `running`
/// slots turned back to `failed` by `record_start_failures`.
async fn parked_without_input() -> Group {
    let shape = Shape {
        one_prompt: false,
        two_prompt: false,
        ..Shape::new()
    };
    let g = group(shape, holding()).await;
    g.start(None).await.unwrap();
    g.wait_status("stuck").await;
    g
}

#[tokio::test]
async fn a_failed_record_of_start_failures_marks_stuck_and_the_next_retry_recovers() {
    let g = parked_without_input().await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_failed_back BEFORE UPDATE ON workflow_state
         WHEN json_extract(NEW.payload, '$.parallel.branches.one.state') = 'failed'
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(matches!(err, RetryTaskError::Enter(_)), "{err}");
    assert_eq!(g.status().await, "stuck");
    let reason = stuck_reason(&g.pool, &g.task_id).await;
    assert!(reason.contains("retry failed"), "{reason}");
    wait_settle_errors(&g, 2).await;

    sqlx::query(sqlx::AssertSqlSafe("DROP TRIGGER no_failed_back"))
        .execute(&g.pool)
        .await
        .unwrap();
    let outcome = retry(&g, RetryMode::Auto).await.unwrap();
    assert_eq!(outcome.branches.len(), 2);
}

#[tokio::test]
async fn a_retry_whose_write_is_ignored_reports_no_workflow_state() {
    let g = parked_without_input().await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER ignore_state BEFORE UPDATE ON workflow_state
         BEGIN SELECT RAISE(IGNORE); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(matches!(err, RetryTaskError::NoWorkflowState), "{err}");
    assert_eq!(snapshot(&g).await, before);
}

#[tokio::test]
async fn a_retry_whose_reopen_is_ignored_reports_not_stuck_and_keeps_running_slots() {
    let g = parked_without_input().await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER ignore_reopen BEFORE UPDATE ON tasks
         BEGIN SELECT RAISE(IGNORE); END",
    ))
    .execute(&g.pool)
    .await
    .unwrap();
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotStuck(s) if s == "stuck"),
        "{err}"
    );
    assert_eq!(g.status().await, "stuck");
    let payload = g.state().await.payload;
    assert_eq!(branch_slot(&payload, "one")["state"], "running");
    assert_eq!(branch_slot(&payload, "two")["state"], "running");
    assert!(g.sessions("one").await.is_empty());
}

#[tokio::test]
async fn a_block_with_an_entry_but_no_branches_is_group_state_missing() {
    let g = parked_without_input().await;
    let state = g.state().await;
    let mut payload = state.payload.clone();
    payload["parallel"]["branches"] = Value::Null;
    workflow_state::update(
        &g.pool,
        &g.task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage.clone(),
            stage_kind: "parallel".to_string(),
            loop_counters: state.loop_counters.clone(),
            payload,
            enters_stage: false,
        },
    )
    .await
    .unwrap();
    let before = snapshot(&g).await;
    let err = retry(&g, RetryMode::Auto).await.unwrap_err();
    assert!(
        matches!(err, RetryTaskError::GroupStateMissing { .. }),
        "{err}"
    );
    assert_eq!(snapshot(&g).await, before);
}

#[tokio::test]
async fn resume_reports_a_live_later_branch_before_a_non_resumable_earlier_one() {
    let g = group(Shape::new(), holding()).await;
    g.start(None).await.unwrap();
    g.wait_sessions("one", 1).await;
    g.wait_sessions("two", 1).await;
    install_no_state_update(&g).await;
    let one = g.sessions("one").await.remove(0);
    g.finish(1, "one", &one.id, reported("bogus", "x")).await;
    // `one` is exited, so only `two` is live; `one` is not resumable.
    sqlx::query("UPDATE sessions SET status = 'exited' WHERE id = ?")
        .bind(&one.id)
        .execute(&g.pool)
        .await
        .unwrap();
    let err = retry(&g, RetryMode::Resume).await.unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::BranchStillActive { branch, .. } if branch == "two"),
        "{err}"
    );
}
