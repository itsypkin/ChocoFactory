//! #353: a stage that starts a new attempt no longer shows the previous
//! attempt's verdict as its live capture. The old text is still rendered into
//! that attempt's first prompt (the clear comes after rendering) and is still
//! readable from the applied history. Isolated fixtures: a scripted stand-in
//! for `claude`, never the real CLI and never a real usage limit.

use super::super::turn::remove_stage_capture;
use super::super::turns::{Claim, TurnSpec};
use super::*;
use crate::context::{self, ContextRequest, Lookup};
use chocofactory_core::context::{AppliedStageResult, ContextResponse};
use chocofactory_core::models::Session;

/// `plan` hands `review` a text; `review` can loop on itself and reads its own
/// previous summary back, the way `planner-turn.md` reads `spec_check`'s.
const FLOW: &str = r#"name: stale-flow
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  plan:
    kind: shell
    command: "printf 'HANDOFF-FROM-PLAN'"
    capture: text
    on: { done: review }
  review:
    kind: agent_turn
    role: reviewer
    prompt_file: review-turn.md
    capture: json
    on: { again: review, done: finished }
  finished:
    kind: terminal
"#;

const PROMPT: &str =
    "Hand-off: {{ stages.plan }}\nEarlier verdict: [{{ stages.review.summary }}]\n";

const OLD: &str = "OLD-REVIEW-MARK first line\nsecond line of the old review\n  - finding one\n";
const NEW: &str = "NEW-REVIEW-TEXT distinct from the old one\nand also multiline\n";
const UNAPPLIED: &str = "UNAPPLIED-REPORT-TEXT never applied\n";

struct Flow {
    pool: SqlitePool,
    dir: TempDir,
    task_id: String,
}

fn flow_in(dir: &Path) -> Arc<WorkflowDefinition> {
    fs::write(dir.join("review-turn.md"), PROMPT).unwrap();
    fs::write(dir.join("stale-flow.yaml"), FLOW).unwrap();
    Arc::new(WorkflowDefinition::parse(FLOW, dir).unwrap())
}

/// The first entry into `review` has no earlier verdict, so its prompt reads
/// `[]`; the second carries OLD back into the prompt.
fn first_entry_marker() -> &'static str {
    "Earlier verdict: []"
}

async fn review_runs(pool: &SqlitePool, task_id: &str) -> Vec<Session> {
    runs_for_stage(pool, task_id, "review").await
}

async fn wait_for_review_runs(pool: &SqlitePool, task_id: &str, n: usize) -> Vec<Session> {
    crate::test_support::wait_until(&format!("{n} review runs"), || async {
        let runs = review_runs(pool, task_id).await;
        if runs.len() == n {
            Ok(runs)
        } else {
            Err(format!("{} runs", runs.len()))
        }
    })
    .await
}

/// What a session was actually sent, as the scripted agent echoed it back.
pub(super) async fn delivered_to(pool: &SqlitePool, session_id: &str) -> String {
    crate::test_support::wait_until("the echoed prompt", || async {
        let text = events::list_for_session(pool, session_id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| e.payload["text"].as_str().map(str::to_string))
            .filter_map(|t| t.strip_prefix("turn text: ").map(str::to_string))
            // The echo is the user message as the CLI received it.
            .filter_map(|t| serde_json::from_str::<Value>(&t).ok())
            .filter_map(|m| {
                m["message"]["content"][0]["text"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            Err("nothing echoed yet".to_string())
        } else {
            Ok(text)
        }
    })
    .await
}

fn live_review(payload: &Value) -> Option<&Value> {
    payload
        .get("stages")
        .and_then(|stages| stages.get("review"))
}

fn release(path: &Path) {
    fs::write(path, "go").unwrap();
}

/// Runs `plan`, then `review` twice: the first entry applies OLD and loops,
/// the second is held (before any report) until `hold` is released, then hits
/// a usage limit. Returns once the second run has been sent its prompt.
async fn held_second_review() -> (Flow, PathBuf, Arc<WorkflowEngine>) {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let hold = dir.join("hold-second-review");
    let binary = named_script_binary(
        &dir,
        "fake-claude-stale",
        json!({"by_prompt": {
            first_entry_marker(): [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "again", "summary": OLD},
                {"op": "result"},
            ],
            "OLD-REVIEW-MARK": [
                {"op": "echo_turn"},
                {"op": "wait_file", "path": hold},
                {"op": "usage_limit"},
            ],
        }}),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    let flow = Flow { pool, dir, task_id };
    wait_for_review_runs(&flow.pool, &flow.task_id, 2).await;
    (flow, hold, engine)
}

async fn second_run(flow: &Flow) -> Session {
    let runs = review_runs(&flow.pool, &flow.task_id).await;
    let delivered_old = |s: &&Session| s.resumed_from.is_none();
    // The second run is the one that was not applied: the first ended `done`.
    let mut runs: Vec<_> = runs.iter().filter(delivered_old).cloned().collect();
    runs.sort_by_key(|s| s.started_at);
    runs.pop().unwrap()
}

#[tokio::test]
async fn review_reentry_clears_live_verdict_through_interruption_and_resume() {
    let (flow, hold, _engine) = held_second_review().await;
    let second = second_run(&flow).await;

    // The old verdict reached the new attempt's prompt, with the hand-off from
    // the other stage, before the clear.
    let delivered = delivered_to(&flow.pool, &second.id).await;
    assert!(
        delivered.contains(OLD),
        "OLD must reach the prompt: {delivered}"
    );
    assert!(delivered.contains("HANDOFF-FROM-PLAN"), "{delivered}");

    // While the second attempt is held, the live capture is gone and the
    // verdict that was applied is still the one recorded.
    let held = state_of(&flow.pool, &flow.task_id).await;
    assert!(
        live_review(&held.payload).is_none(),
        "the previous attempt's verdict is still shown as live: {:?}",
        live_review(&held.payload)
    );
    assert_eq!(held.payload["stages"]["plan"], json!("HANDOFF-FROM-PLAN"));
    let results = stage_results(&flow.pool, &flow.task_id).await;
    let applied_before: Vec<_> = results.iter().map(|e| e.id.clone()).collect();
    let first_applied = AppliedStageResult::from_event(
        results
            .iter()
            .find(|e| e.payload["source_stage"] == "review")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first_applied.capture["summary"], json!(OLD));
    assert_eq!(
        held.payload["current_result_id"],
        json!(results.last().unwrap().id)
    );

    // The usage limit interrupts the attempt: stuck, nothing advanced, and
    // still no verdict anyone could mistake for this attempt's.
    release(&hold);
    wait_until_task_status(&flow.pool, &flow.task_id, "stuck").await;
    let stuck = state_of(&flow.pool, &flow.task_id).await;
    assert_eq!(stuck.current_stage, "review");
    assert!(
        live_review(&stuck.payload).is_none(),
        "a stuck stage shows the old verdict: {:?}",
        live_review(&stuck.payload)
    );
    let second_after = sessions::get(&flow.pool, &second.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_after.end_reason, Some(SessionEndReason::Interrupted));
    let results_after = stage_results(&flow.pool, &flow.task_id).await;
    assert_eq!(
        results_after
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        applied_before,
        "an interrupted attempt must not add a result"
    );
    assert_eq!(
        stuck.payload["current_result_id"],
        held.payload["current_result_id"]
    );

    // A supported resume continues the same conversation, is held before it
    // reports, and still shows no verdict.
    let hold2 = flow.dir.join("hold-resumed-review");
    let binary = named_script_binary(
        &flow.dir,
        "fake-claude-stale-resumed",
        json!([
            {"op": "echo_turn"},
            {"op": "wait_file", "path": hold2},
            {"op": "report", "outcome": "done", "summary": NEW},
            {"op": "result"},
        ]),
    );
    let engine2 = engine_with_adapter_and_workflows_dir(flow.pool.clone(), &binary, &flow.dir);
    let outcome = engine2
        .retry_task(&flow.task_id, RetryMode::Auto)
        .await
        .unwrap();
    assert!(outcome.resumed);
    let runs = wait_for_review_runs(&flow.pool, &flow.task_id, 3).await;
    let resumed = runs.iter().find(|r| r.resumed_from.is_some()).unwrap();
    assert_eq!(resumed.resumed_from.as_deref(), Some(second.id.as_str()));
    assert!(second_after.adapter_session_id.is_some());
    assert_eq!(resumed.adapter_session_id, second_after.adapter_session_id);
    let during = state_of(&flow.pool, &flow.task_id).await;
    assert!(
        live_review(&during.payload).is_none(),
        "{:?}",
        during.payload
    );

    // The new verdict appears whole, once, and is chained to the old one.
    release(&hold2);
    wait_until_task_status(&flow.pool, &flow.task_id, "closed").await;
    let done = state_of(&flow.pool, &flow.task_id).await;
    assert_eq!(done.payload["stages"]["review"]["summary"], json!(NEW));
    let results = stage_results(&flow.pool, &flow.task_id).await;
    assert_eq!(results.len(), applied_before.len() + 1);
    let new_result = AppliedStageResult::from_event(results.last().unwrap()).unwrap();
    assert_eq!(new_result.source_stage, "review");
    assert_eq!(new_result.outcome, "done");
    assert_eq!(new_result.destination_stage, "finished");
    assert_eq!(new_result.capture["summary"], json!(NEW));
    assert_eq!(
        new_result.previous_result_id.as_deref(),
        applied_before.last().map(String::as_str)
    );
}

#[tokio::test]
async fn clearing_live_capture_preserves_applied_history() {
    let (flow, hold, _engine) = held_second_review().await;
    // The live capture is already gone while the second attempt is held.
    let held = state_of(&flow.pool, &flow.task_id).await;
    assert!(live_review(&held.payload).is_none());

    // The earlier verdict is still there, whole, by stage and by result id.
    let by_stage = context::lookup(
        &flow.pool,
        &flow.task_id,
        &ContextRequest::StageByName {
            stage: "review".to_string(),
            outcome: None,
            anchor_id: None,
        },
    )
    .await
    .unwrap();
    let Lookup::Found(ContextResponse::StageResult { result, .. }) = by_stage else {
        panic!("the old applied review is not readable: {by_stage:?}");
    };
    assert_eq!(result.capture["summary"], json!(OLD));
    let by_id = context::lookup(
        &flow.pool,
        &flow.task_id,
        &ContextRequest::StageById {
            result_id: result.result_id.clone(),
            anchor_id: None,
        },
    )
    .await
    .unwrap();
    let Lookup::Found(ContextResponse::StageResult { result: again, .. }) = by_id else {
        panic!("the old applied review is not readable by id: {by_id:?}");
    };
    assert_eq!(again.capture["summary"], json!(OLD));
    release(&hold);
}

#[tokio::test]
async fn fresh_retry_preserves_self_reference_contract() {
    let (flow, hold, _engine) = held_second_review().await;
    let second = second_run(&flow).await;
    // First delivery of the entry: OLD arrived.
    assert!(delivered_to(&flow.pool, &second.id).await.contains(OLD));
    release(&hold);
    wait_until_task_status(&flow.pool, &flow.task_id, "stuck").await;

    // A fresh re-render after the clear has nothing to put in its own slot,
    // but keeps the rest of the prompt (accepted limitation).
    let binary = named_script_binary(
        &flow.dir,
        "fake-claude-stale-fresh",
        json!([{"op": "echo_turn"}, {"op": "wait_file", "path": flow.dir.join("never")}]),
    );
    let engine = engine_with_adapter_and_workflows_dir(flow.pool.clone(), &binary, &flow.dir);
    let outcome = engine
        .retry_task(&flow.task_id, RetryMode::Fresh)
        .await
        .unwrap();
    assert!(!outcome.resumed);
    let runs = wait_for_review_runs(&flow.pool, &flow.task_id, 3).await;
    let fresh = runs
        .iter()
        .find(|r| r.id != second.id && r.started_at > second.started_at)
        .unwrap();
    assert!(fresh.resumed_from.is_none());
    let delivered = delivered_to(&flow.pool, &fresh.id).await;
    assert!(delivered.contains("Earlier verdict: []"), "{delivered}");
    assert!(!delivered.contains("OLD-REVIEW-MARK"), "{delivered}");
    assert!(delivered.contains("HANDOFF-FROM-PLAN"), "{delivered}");
    let state = state_of(&flow.pool, &flow.task_id).await;
    assert!(live_review(&state.payload).is_none());
}

/// The `is_error` flag of every `report_outcome` result recorded for a
/// session: one entry per result the daemon has stored.
async fn recorded_report_results(pool: &SqlitePool, session_id: &str) -> Vec<bool> {
    events::list_for_session(pool, session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::ToolResult)
        .filter(|e| e.payload["tool_use_id"] == "toolu_report_1")
        .map(|e| e.payload["is_error"].as_bool().unwrap_or(false))
        .collect()
}

#[tokio::test]
async fn accepted_unapplied_report_never_becomes_live() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let hold = dir.join("hold-after-report");
    let binary = named_script_binary(
        &dir,
        "fake-claude-unapplied",
        json!({"by_prompt": {
            first_entry_marker(): [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "again", "summary": OLD},
                {"op": "result"},
            ],
            "OLD-REVIEW-MARK": [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "done", "summary": UNAPPLIED},
                {"op": "wait_file", "path": hold},
                {"op": "usage_limit"},
            ],
        }}),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    let runs = wait_for_review_runs(&pool, &task_id, 2).await;
    let second = runs.iter().max_by_key(|s| s.started_at).cloned().unwrap();

    // The second report was accepted and stored, and the turn has not ended:
    // this is the condition the rest of the test is about.
    crate::test_support::wait_until("the second report's accepted result", || async {
        match recorded_report_results(&pool, &second.id).await.as_slice() {
            [false] => Ok(()),
            other => Err(format!("report results so far: {other:?}")),
        }
    })
    .await;
    let before_results = stage_results(&pool, &task_id).await;
    let before = state_of(&pool, &task_id).await;
    assert_eq!(before.current_stage, "review");
    assert_eq!(
        before_results.len(),
        2,
        "plan→review and the one applied review"
    );
    assert_eq!(
        before.payload["current_result_id"],
        json!(before_results.last().unwrap().id)
    );
    assert!(
        live_review(&before.payload).is_none(),
        "an accepted-but-unapplied report is shown as live: {:?}",
        live_review(&before.payload)
    );

    // The usage limit interrupts the turn after its accepted report: nothing
    // is applied, nothing goes live, and the history is exactly as it was.
    release(&hold);
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "review");
    assert!(
        live_review(&state.payload).is_none(),
        "an accepted-but-unapplied report is shown as live: {:?}",
        live_review(&state.payload)
    );
    assert_eq!(
        state.payload["current_result_id"],
        before.payload["current_result_id"]
    );
    let results = stage_results(&pool, &task_id).await;
    assert_eq!(
        results.iter().map(|e| &e.id).collect::<Vec<_>>(),
        before_results.iter().map(|e| &e.id).collect::<Vec<_>>()
    );
    assert!(
        !results
            .iter()
            .any(|e| e.payload.to_string().contains("UNAPPLIED-REPORT-TEXT")),
        "the unapplied report was recorded as applied"
    );
}

/// Fails exactly the write that removes `review`'s live capture.
async fn fail_the_capture_clear(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TRIGGER fail_clear BEFORE UPDATE ON workflow_state
         WHEN json_extract(old.payload, '$.stages.review') IS NOT NULL
          AND json_extract(new.payload, '$.stages.review') IS NULL
         BEGIN SELECT RAISE(ABORT, 'injected clear failure'); END",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_capture_clear_starts_no_agent() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let binary = named_script_binary(
        &dir,
        "fake-claude-clear-fails",
        json!({"by_prompt": {
            first_entry_marker(): [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "again", "summary": OLD},
                {"op": "result"},
            ],
            "OLD-REVIEW-MARK": [{"op": "echo_turn"}],
        }}),
    );
    fail_the_capture_clear(&pool).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .unwrap()
            .contains("injected clear failure"),
        "the clear was not the write that failed"
    );
    assert_eq!(
        review_runs(&pool, &task_id).await.len(),
        1,
        "an agent was started"
    );
    // The previous snapshot is untouched: a failed write loses nothing.
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.payload["stages"]["review"]["summary"], json!(OLD));
    assert_eq!(state.payload["stages"]["plan"], json!("HANDOFF-FROM-PLAN"));
}

#[tokio::test]
async fn clearing_a_task_with_no_workflow_state_is_refused() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter(pool.clone(), "unused");
    let task_id = seed_task(&pool, &def.name).await;
    let err = engine
        .clear_stage_capture(&task_id, &def, "review")
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::NoWorkflowState), "{err:?}");
}

#[tokio::test]
async fn clearing_removes_only_the_target_and_changes_nothing_else() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter(pool.clone(), "unused");
    let task_id = seed_task(&pool, &def.name).await;
    let payload = json!({
        "task": {"title": "T", "input": "do it"},
        "stages": {
            "review": {"outcome": "again", "summary": OLD},
            "plan": "HANDOFF-FROM-PLAN",
            "other_review": {"outcome": "done", "summary": "kept"},
        },
        "arrival": {"from": "review", "outcome": "again"},
        "finished_stages": ["plan"],
        "left_at": {"plan": "2026-01-01T00:00:00Z"},
        "current_result_id": "result-pointer",
    });
    let created = workflow_state::create(&pool, &task_id, "review", "agent_turn", payload.clone())
        .await
        .unwrap();
    sqlx::query("UPDATE workflow_state SET loop_counters = ? WHERE task_id = ?")
        .bind(json!({"review": 2}).to_string())
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();

    engine
        .clear_stage_capture(&task_id, &def, "review")
        .await
        .unwrap();

    let after = state_of(&pool, &task_id).await;
    let mut expected = payload;
    expected["stages"].as_object_mut().unwrap().remove("review");
    assert_eq!(after.payload, expected);
    assert_eq!(after.loop_counters, json!({"review": 2}));
    assert_eq!(after.current_stage, "review");
    assert_eq!(after.stage_kind.as_deref(), Some("agent_turn"));
    assert_eq!(after.stage_entered_at, created.stage_entered_at);

    // Nothing left to remove: nothing is written.
    engine
        .clear_stage_capture(&task_id, &def, "review")
        .await
        .unwrap();
    let again = state_of(&pool, &task_id).await;
    assert_eq!(again.updated_at, after.updated_at);
}

#[test]
fn removing_a_capture_covers_every_shape_of_payload() {
    for target in [
        json!(null),
        json!({"summary": "x"}),
        json!("text"),
        json!([1, 2]),
        json!(7),
        json!(true),
    ] {
        let mut payload = json!({"stages": {"review": target, "plan": "keep"}, "k": 1});
        assert!(remove_stage_capture(&mut payload, "review"));
        assert_eq!(payload, json!({"stages": {"plan": "keep"}, "k": 1}));
    }
    for untouched in [
        json!({"stages": {"plan": "keep"}}),
        json!({"stages": null}),
        json!({"stages": "text"}),
        json!({"stages": [1]}),
        json!({"other": 1}),
        json!(null),
        json!("text"),
        json!([1]),
    ] {
        let mut payload = untouched.clone();
        assert!(!remove_stage_capture(&mut payload, "review"));
        assert_eq!(payload, untouched);
    }
}

// ---- the launch boundary ---------------------------------------------------

/// Seeds the state a second entry into `review` starts from: the other
/// stage's hand-off, the previous verdict, and a pointer to the applied history.
async fn seed_review_state(pool: &SqlitePool, task_id: &str) {
    workflow_state::create(
        pool,
        task_id,
        "review",
        "agent_turn",
        json!({
            "stages": {
                "plan": "HANDOFF-FROM-PLAN",
                "review": {"outcome": "again", "summary": OLD},
            },
            "current_result_id": "result-pointer",
        }),
    )
    .await
    .unwrap();
}

/// Prepares `review`'s turn from the persisted state and launches it, running
/// `between` once the turn is prepared and before the launch.
async fn launch_review(
    engine: &Arc<WorkflowEngine>,
    def: &Arc<WorkflowDefinition>,
    task_id: &str,
    between: Option<&str>,
) -> Result<String, EngineError> {
    let state = workflow_state::get(&engine.pool, task_id)
        .await?
        .ok_or(EngineError::NoWorkflowState)?;
    let entry = StageEntry {
        task_id,
        definition: def,
        stage_name: "review",
        stage_def: &def.stages["review"],
        payload: &state.payload,
        input: None,
        resume: None,
        branch: None,
    };
    let prepared = engine.prepare_turn(&entry).await?;
    if let Some(sql) = between {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&engine.pool)
            .await?;
    }
    engine.launch_turn(prepared, None, None).await
}

/// What a refused launch must leave exactly as it found it.
async fn untouched(pool: &SqlitePool, task_id: &str) -> (Value, Value, String, usize, usize) {
    let state = state_of(pool, task_id).await;
    (
        state.payload,
        state.loop_counters,
        state.current_stage,
        sessions::list_for_task(pool, task_id).await.unwrap().len(),
        stage_results(pool, task_id).await.len(),
    )
}

#[tokio::test]
async fn a_launch_whose_workflow_state_was_deleted_after_preparation_is_refused() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let task_id = seed_task(&pool, &def.name).await;
    seed_review_state(&pool, &task_id).await;

    let delete = format!("DELETE FROM workflow_state WHERE task_id = '{task_id}'");
    let err = launch_review(&engine, &def, &task_id, Some(&delete))
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::NoWorkflowState), "{err:?}");
    assert!(
        sessions::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .is_empty(),
        "an agent was started for a task with no state"
    );
    assert!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_failed_clear_reaches_the_caller_and_changes_nothing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let task_id = seed_task(&pool, &def.name).await;
    seed_review_state(&pool, &task_id).await;
    fail_the_capture_clear(&pool).await;
    let before = untouched(&pool, &task_id).await;

    let err = launch_review(&engine, &def, &task_id, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Db(_)) && err.to_string().contains("injected clear failure"),
        "{err:?}"
    );
    let after = untouched(&pool, &task_id).await;
    assert_eq!(after, before);
    assert_eq!(after.0["stages"]["review"]["summary"], json!(OLD));
    assert_eq!(after.0["current_result_id"], json!("result-pointer"));
    assert_eq!(after.3, 0, "an agent was started");
}

#[tokio::test]
async fn a_prompt_that_cannot_render_keeps_the_previous_capture() {
    for (what, prompt) in [
        ("a missing prompt file", None),
        ("a malformed placeholder", Some("verdict: {{ stages.review")),
    ] {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let def = flow_in(&dir);
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
        let task_id = seed_task(&pool, &def.name).await;
        seed_review_state(&pool, &task_id).await;
        match prompt {
            None => fs::remove_file(dir.join("review-turn.md")).unwrap(),
            Some(text) => fs::write(dir.join("review-turn.md"), text).unwrap(),
        }
        let before = untouched(&pool, &task_id).await;

        let err = launch_review(&engine, &def, &task_id, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, EngineError::Io(_) | EngineError::Template { .. }),
            "{what}: {err:?}"
        );
        let after = untouched(&pool, &task_id).await;
        assert_eq!(after, before, "{what}");
        assert_eq!(after.0["stages"]["review"]["summary"], json!(OLD), "{what}");
    }
}

#[tokio::test]
async fn a_baseline_that_cannot_be_taken_keeps_the_previous_capture() {
    // A read-only role's baseline is taken before the clear. Its worktree is
    // gone, so it cannot be taken.
    let run = start_read_only_task("reviewer", false, json!({}), ro_steps(&[])).await;
    wait_until_stage(&run.pool, &run.task_id, "verified").await;
    let state = state_of(&run.pool, &run.task_id).await;
    let mut payload = state.payload.clone();
    payload["stages"] = json!({"review": {"outcome": "done", "summary": OLD}});
    workflow_state::update(
        &run.pool,
        &run.task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: "review".to_string(),
            stage_kind: "agent_turn".to_string(),
            loop_counters: state.loop_counters,
            payload,
            enters_stage: false,
        },
    )
    .await
    .unwrap()
    .unwrap();
    fs::remove_dir_all(&run.worktree_dir).unwrap();
    let before = untouched(&run.pool, &run.task_id).await;

    let err = launch_review(&run._engine, &run.def, &run.task_id, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Worktree(_) | EngineError::ReadOnlyBaseline { .. }
        ),
        "{err:?}"
    );
    let after = untouched(&run.pool, &run.task_id).await;
    assert_eq!(after, before);
    assert_eq!(after.0["stages"]["review"]["summary"], json!(OLD));
}

#[tokio::test]
async fn a_launch_that_fails_after_the_clear_leaves_it_cleared_and_a_fresh_retry_starts() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let binary = named_script_binary(
        &dir,
        "fake-claude-insert-fails",
        json!({"by_prompt": {
            first_entry_marker(): [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "again", "summary": OLD},
                {"op": "result"},
            ],
        }}),
    );
    // The first `review` session is fine; the second one's row cannot be
    // written, after the previous capture has been cleared.
    sqlx::query(
        "CREATE TRIGGER no_second_review BEFORE INSERT ON sessions
         WHEN NEW.stage = 'review' AND EXISTS (SELECT 1 FROM sessions WHERE stage = 'review')
         BEGIN SELECT RAISE(ABORT, 'injected session failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .unwrap()
            .contains("injected session failure"),
        "the session insert was not what failed"
    );
    assert_eq!(review_runs(&pool, &task_id).await.len(), 1);
    let stuck = state_of(&pool, &task_id).await;
    assert!(
        live_review(&stuck.payload).is_none(),
        "the capture was not cleared before the failed launch: {:?}",
        live_review(&stuck.payload)
    );
    assert_eq!(stuck.payload["stages"]["plan"], json!("HANDOFF-FROM-PLAN"));
    let applied = stage_results(&pool, &task_id).await;
    assert_eq!(applied.len(), 2);

    // The fault clears and the task is retried from scratch: the attempt
    // starts, its prompt lacks the old own-text (the accepted limitation) and
    // keeps the rest.
    sqlx::query("DROP TRIGGER no_second_review")
        .execute(&pool)
        .await
        .unwrap();
    let binary = named_script_binary(
        &dir,
        "fake-claude-after-insert-fails",
        json!([{"op": "echo_turn"}, {"op": "wait_file", "path": dir.join("never")}]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let outcome = engine.retry_task(&task_id, RetryMode::Fresh).await.unwrap();
    assert!(!outcome.resumed);
    let runs = wait_for_review_runs(&pool, &task_id, 2).await;
    let fresh = runs.iter().max_by_key(|s| s.started_at).unwrap();
    let delivered = delivered_to(&pool, &fresh.id).await;
    assert!(delivered.contains("Earlier verdict: []"), "{delivered}");
    assert!(delivered.contains("HANDOFF-FROM-PLAN"), "{delivered}");
    assert_eq!(stage_results(&pool, &task_id).await.len(), applied.len());
}

#[tokio::test]
async fn a_launch_dropped_while_clearing_leaves_a_settled_state_and_the_next_one_starts() {
    let dir = tempdir();
    let db_path = dir.join("abandon.db");
    let pool = crate::db::connect(&db_path).await.unwrap();
    let def = flow_in(&dir);
    let binary = named_script_binary(
        &dir,
        "fake-claude-after-abandon",
        json!([
            {"op": "echo_turn"},
            {"op": "report", "outcome": "done", "summary": NEW},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    seed_review_state(&pool, &task_id).await;
    let before = untouched(&pool, &task_id).await;

    // Another connection holds the database's write lock, so the launch's
    // clearing write has to wait for it.
    let observer = crate::db::connect(&db_path).await.unwrap();
    let mut writer = observer.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *writer)
        .await
        .unwrap();

    // The slot is taken exactly as the production caller takes it, and goes
    // with the dropped launch.
    let spec = TurnSpec {
        stage: "review",
        group: None,
        resume_session_id: None,
    };
    let Claim::Start(permit) = engine.turn_gate().claim(&task_id, &spec).await.unwrap() else {
        panic!("the slot was free");
    };
    assert_eq!(engine.running_total(), 1);
    let launch = {
        let engine = Arc::clone(&engine);
        let def = Arc::clone(&def);
        let task_id = task_id.clone();
        tokio::spawn(async move {
            let state = workflow_state::get(&engine.pool, &task_id)
                .await
                .unwrap()
                .unwrap();
            let entry = StageEntry {
                task_id: &task_id,
                definition: &def,
                stage_name: "review",
                stage_def: &def.stages["review"],
                payload: &state.payload,
                input: None,
                resume: None,
                branch: None,
            };
            let prepared = engine.prepare_turn(&entry).await.unwrap();
            engine.launch_turn(prepared, Some(permit), None).await
        })
    };
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    assert!(
        !launch.is_finished(),
        "the launch did not wait for the write lock"
    );
    launch.abort();
    assert!(launch.await.unwrap_err().is_cancelled());

    // Dropping the launch does not cancel the statement SQLite is already
    // waiting on in its worker thread, so once the lock is released the clear
    // may still land. Either way the one write is atomic and nothing else
    // moves: another connection sees the state as it was, or as it was without
    // the cleared capture, and no session and no result exist.
    sqlx::query("ROLLBACK").execute(&mut *writer).await.unwrap();
    drop(writer);
    tokio::time::sleep(StdDuration::from_millis(700)).await;
    let (mut payload, counters, stage, sessions_after, results_after) =
        untouched(&observer, &task_id).await;
    let mut expected = before.0.clone();
    if payload["stages"].get("review").is_none() {
        expected["stages"].as_object_mut().unwrap().remove("review");
    }
    assert_eq!(
        payload, expected,
        "the dropped clear changed more than the capture"
    );
    payload["stages"] = Value::Null;
    assert_eq!(
        (counters, stage, sessions_after, results_after),
        (before.1.clone(), before.2.clone(), 0, 0),
        "{payload}"
    );
    assert_eq!(
        engine.running_total(),
        0,
        "the dropped launch kept its slot"
    );

    // The lock, the slot and the connection are all reusable: the same turn
    // launches, clears the capture and runs to its end.
    let Claim::Start(permit) = engine.turn_gate().claim(&task_id, &spec).await.unwrap() else {
        panic!("the slot was not freed");
    };
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    let entry = StageEntry {
        task_id: &task_id,
        definition: &def,
        stage_name: "review",
        stage_def: &def.stages["review"],
        payload: &state.payload,
        input: None,
        resume: None,
        branch: None,
    };
    let prepared = engine.prepare_turn(&entry).await.unwrap();
    let session_id = engine
        .launch_turn(prepared, Some(permit), None)
        .await
        .unwrap();
    delivered_to(&pool, &session_id).await;
    wait_until_task_status(&pool, &task_id, "closed").await;
    let done = state_of(&observer, &task_id).await;
    assert_eq!(done.payload["stages"]["review"]["summary"], json!(NEW));
}

#[tokio::test]
async fn a_legacy_row_without_a_stage_kind_gets_it_filled_in_by_the_clear() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter(pool.clone(), "unused");
    let task_id = seed_task(&pool, &def.name).await;
    seed_review_state(&pool, &task_id).await;
    sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(state_of(&pool, &task_id).await.stage_kind, None);

    engine
        .clear_stage_capture(&task_id, &def, "review")
        .await
        .unwrap();
    let after = state_of(&pool, &task_id).await;
    assert_eq!(after.stage_kind.as_deref(), Some("agent_turn"));
    assert!(live_review(&after.payload).is_none());
    assert_eq!(after.payload["stages"]["plan"], json!("HANDOFF-FROM-PLAN"));
    assert_eq!(after.current_stage, "review");
}

#[tokio::test]
async fn a_legacy_row_in_an_unknown_stage_is_refused_and_keeps_its_capture() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
    let engine = engine_with_adapter(pool.clone(), "unused");
    let task_id = seed_task(&pool, &def.name).await;
    seed_review_state(&pool, &task_id).await;
    sqlx::query(
        "UPDATE workflow_state SET stage_kind = NULL, current_stage = 'ghost' WHERE task_id = ?",
    )
    .bind(&task_id)
    .execute(&pool)
    .await
    .unwrap();
    let before = state_of(&pool, &task_id).await;

    let err = engine
        .clear_stage_capture(&task_id, &def, "review")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::UnknownStage(s) if s == "ghost"),
        "{err:?}"
    );
    let after = state_of(&pool, &task_id).await;
    assert_eq!(after.payload, before.payload);
    assert_eq!(after.stage_kind, None);
}

// ---- lifecycle -------------------------------------------------------------

/// The planned workflow's loop, with the real `planner-turn.md` as the prompt:
/// `spec_check` asks, a human answers, `spec_check` runs again and reads its
/// own previous report back.
const PLANNED: &str = r#"name: planned-flow
roles:
  planner:
    cli: claude
    model: sonnet
stages:
  spec_check:
    kind: agent_turn
    role: planner
    prompt_file: planner-turn.md
    capture: json
    on: { ready: finished, needs_input: spec_questions }
  spec_questions:
    kind: human_gate
    capture: text
    on: { resumed: spec_check }
  finished:
    kind: terminal
"#;

const DRAFT: &str = "DRAFT-SPEC-MARK checks and questions\n- question one\n- question two\n";
const ANSWER: &str = "HUMAN-ANSWER-MARK take option B";
const FINAL_SPEC: &str = "FINAL-SPEC-MARK the folded-in spec\n";

#[tokio::test]
async fn the_planner_rerun_reads_its_previous_report_then_shows_none_until_it_reports() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    fs::write(
        dir.join("planner-turn.md"),
        include_str!("../../../../workflows/prompts/planner-turn.md"),
    )
    .unwrap();
    fs::write(dir.join("planned-flow.yaml"), PLANNED).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(PLANNED, &dir).unwrap());
    let hold = dir.join("hold-planner");
    let binary = named_script_binary(
        &dir,
        "fake-claude-planner",
        json!({"by_prompt": {
            "HUMAN-ANSWER-MARK": [
                {"op": "echo_turn"},
                {"op": "wait_file", "path": hold},
                {"op": "usage_limit"},
            ],
            "Task:": [
                {"op": "echo_turn"},
                {"op": "report", "outcome": "needs_input", "summary": DRAFT},
                {"op": "result"},
            ],
        }}),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "spec_questions").await;
    let asked = state_of(&pool, &task_id).await;
    assert_eq!(
        asked.payload["stages"]["spec_check"]["summary"],
        json!(DRAFT)
    );
    let applied_before: Vec<String> = stage_results(&pool, &task_id)
        .await
        .iter()
        .map(|e| e.id.clone())
        .collect();
    assert_eq!(applied_before.len(), 1, "spec_check → spec_questions");

    engine
        .send_message_or_resume(&task_id, ANSWER)
        .await
        .unwrap();
    let second = crate::test_support::wait_until("the second planner run", || async {
        let runs = runs_for_stage(&pool, &task_id, "spec_check").await;
        if runs.len() == 2 {
            Ok(runs.into_iter().max_by_key(|s| s.started_at).unwrap())
        } else {
            Err(format!("{} runs", runs.len() + 1))
        }
    })
    .await;

    // The real prompt carried the previous report and the answer to the new
    // attempt; the live copy of that report is already gone.
    let delivered = delivered_to(&pool, &second.id).await;
    assert!(delivered.contains(DRAFT), "{delivered}");
    assert!(delivered.contains(ANSWER), "{delivered}");
    let held = state_of(&pool, &task_id).await;
    assert!(
        held.payload["stages"].get("spec_check").is_none(),
        "the previous report is shown as live: {:?}",
        held.payload["stages"]
    );
    assert_eq!(held.payload["stages"]["spec_questions"], json!(ANSWER));
    let applied_held: Vec<String> = stage_results(&pool, &task_id)
        .await
        .iter()
        .map(|e| e.id.clone())
        .collect();
    assert_eq!(
        applied_held.len(),
        2,
        "the gate's answer is the second result"
    );
    assert_eq!(held.payload["current_result_id"], json!(applied_held[1]));

    // The interrupted attempt keeps it absent; the previous report is still
    // the applied result of its own departure.
    release(&hold);
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let stuck = state_of(&pool, &task_id).await;
    assert!(stuck.payload["stages"].get("spec_check").is_none());
    let old_result = AppliedStageResult::from_event(
        stage_results(&pool, &task_id)
            .await
            .iter()
            .find(|e| e.payload["source_stage"] == "spec_check")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(old_result.capture["summary"], json!(DRAFT));

    // Resumed, it reports the folded-in spec. Both texts are then readable by
    // the identity they were recorded under, and the new one is chained to the
    // old history.
    let binary = named_script_binary(
        &dir,
        "fake-claude-planner-resumed",
        json!([
            {"op": "echo_turn"},
            {"op": "report", "outcome": "ready", "summary": FINAL_SPEC},
            {"op": "result"},
        ]),
    );
    let engine2 = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine2.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;
    let done = state_of(&pool, &task_id).await;
    assert_eq!(
        done.payload["stages"]["spec_check"]["summary"],
        json!(FINAL_SPEC)
    );
    let results = stage_results(&pool, &task_id).await;
    let new_result = AppliedStageResult::from_event(results.last().unwrap()).unwrap();
    assert_eq!(new_result.outcome, "ready");
    assert_eq!(
        new_result.previous_result_id.as_deref(),
        applied_held.last().map(String::as_str)
    );
    for (id, text) in [
        (&old_result.result_id, DRAFT),
        (&new_result.result_id, FINAL_SPEC),
    ] {
        let by_id = context::lookup(
            &pool,
            &task_id,
            &ContextRequest::StageById {
                result_id: id.clone(),
                anchor_id: None,
            },
        )
        .await
        .unwrap();
        let Lookup::Found(ContextResponse::StageResult { result, .. }) = by_id else {
            panic!("{id} is not readable: {by_id:?}");
        };
        assert_eq!(result.capture["summary"], json!(text));
    }
}

#[tokio::test]
async fn a_restart_sweep_of_a_launched_interrupted_attempt_keeps_the_capture_absent() {
    let (flow, hold, _engine) = held_second_review().await;
    release(&hold);
    wait_until_task_status(&flow.pool, &flow.task_id, "stuck").await;
    // Put the task back as a daemon crash would have left it: open, with the
    // attempt's session gone.
    sqlx::query("UPDATE tasks SET status = 'open', stuck_reason = NULL WHERE id = ?")
        .bind(&flow.task_id)
        .execute(&flow.pool)
        .await
        .unwrap();
    let hold2 = flow.dir.join("hold-after-sweep");
    let binary = named_script_binary(
        &flow.dir,
        "fake-claude-after-sweep",
        json!([
            {"op": "echo_turn"},
            {"op": "wait_file", "path": hold2},
            {"op": "report", "outcome": "done", "summary": NEW},
            {"op": "result"},
        ]),
    );
    let restarted = engine_with_adapter_and_workflows_dir(flow.pool.clone(), &binary, &flow.dir);
    let parked = restarted.park_interrupted_turns().await.unwrap();
    assert_eq!(parked.agent_turns, 1, "{parked:?}");
    wait_until_task_status(&flow.pool, &flow.task_id, "stuck").await;
    let swept = state_of(&flow.pool, &flow.task_id).await;
    assert!(
        live_review(&swept.payload).is_none(),
        "the sweep restored a capture: {:?}",
        live_review(&swept.payload)
    );
    let applied_before = stage_results(&flow.pool, &flow.task_id).await.len();

    restarted
        .retry_task(&flow.task_id, RetryMode::Fresh)
        .await
        .unwrap();
    let runs = wait_for_review_runs(&flow.pool, &flow.task_id, 3).await;
    assert!(runs.iter().all(|r| r.resumed_from.is_none()));
    let during = state_of(&flow.pool, &flow.task_id).await;
    assert!(
        live_review(&during.payload).is_none(),
        "{:?}",
        during.payload
    );
    assert_eq!(
        stage_results(&flow.pool, &flow.task_id).await.len(),
        applied_before
    );
    release(&hold2);
    wait_until_task_status(&flow.pool, &flow.task_id, "closed").await;
    let done = state_of(&flow.pool, &flow.task_id).await;
    assert_eq!(done.payload["stages"]["review"]["summary"], json!(NEW));
}
