//! #353: a stage that starts a new attempt no longer shows the previous
//! attempt's verdict as its live capture. The old text is still rendered into
//! that attempt's first prompt (the clear comes after rendering) and is still
//! readable from the applied history. Isolated fixtures: a scripted stand-in
//! for `claude`, never the real CLI and never a real usage limit.

use super::super::turn::remove_stage_capture;
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

#[tokio::test]
async fn accepted_unapplied_report_never_becomes_live() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = flow_in(&dir);
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
                {"op": "usage_limit"},
            ],
        }}),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let task_id = seed_task(&pool, &def.name).await;
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "review");
    assert!(
        live_review(&state.payload).is_none(),
        "an accepted-but-unapplied report is shown as live: {:?}",
        live_review(&state.payload)
    );
    let results = stage_results(&pool, &task_id).await;
    assert_eq!(results.len(), 2, "plan→review and the one applied review");
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
