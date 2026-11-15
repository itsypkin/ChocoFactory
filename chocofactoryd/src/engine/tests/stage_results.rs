//! Committed stage results: every departure records one linked result in the
//! same transaction as the move, with the full capture, the actual
//! destination and a link to the departure before it. These are isolated
//! fixtures (shell commands, a scripted stand-in for `claude`, replies typed
//! into a gate), not runs of the real CLI.

use super::*;
use crate::context::{self, ContextRequest, Lookup};
use chocofactory_core::context::{AppliedStageResult, ContextResponse, UnavailableReason};
use chocofactory_core::models::{Event, WorkflowState};

/// A gate that can loop on itself, be redirected by its loop guard, and
/// finish. Its replies carry marker lines.
const GATE_FLOW: &str = r#"name: sr-gate
stages:
  gate:
    kind: human_gate
    capture: text
    markers:
      - line: /again
        then: again
      - line: /approve
        then: approved
    on: { again: gate, approved: finished }
    loop_guard: { on: again, max: 1, then: escalated }
  escalated:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;

struct GateFlow {
    pool: SqlitePool,
    engine: Arc<WorkflowEngine>,
    def: Arc<WorkflowDefinition>,
    task_id: String,
    _dir: TempDir,
}

async fn gate_flow() -> GateFlow {
    gate_flow_from(GATE_FLOW, connect_in_memory().await.unwrap(), tempdir()).await
}

async fn gate_flow_from(yaml: &str, pool: SqlitePool, dir: TempDir) -> GateFlow {
    fs::write(dir.join("sr-gate.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    GateFlow {
        pool,
        engine,
        def,
        task_id,
        _dir: dir,
    }
}

impl GateFlow {
    async fn reply(&self, text: &str) -> Result<(), SendMessageOrResumeError> {
        self.engine
            .send_message_or_resume(&self.task_id, text)
            .await
    }

    async fn results(&self) -> Vec<Event> {
        stage_results(&self.pool, &self.task_id).await
    }

    async fn state(&self) -> chocofactory_core::models::WorkflowState {
        state_of(&self.pool, &self.task_id).await
    }
}

fn result_of(event: &Event) -> AppliedStageResult {
    AppliedStageResult::from_event(event).unwrap()
}

async fn ask(pool: &SqlitePool, task_id: &str, request: ContextRequest) -> ContextResponse {
    match context::lookup(pool, task_id, &request).await.unwrap() {
        Lookup::Found(answer) | Lookup::UnknownId(answer) => answer,
        Lookup::NoSuchTask => panic!("no such task {task_id}"),
    }
}

fn found(answer: ContextResponse) -> AppliedStageResult {
    match answer {
        ContextResponse::StageResult { result, .. } => result,
        other => panic!("expected an applied result, got {other:?}"),
    }
}

fn by_name(stage: &str, outcome: Option<&str>, anchor: Option<&str>) -> ContextRequest {
    ContextRequest::StageByName {
        stage: stage.to_string(),
        outcome: outcome.map(str::to_string),
        anchor_id: anchor.map(str::to_string),
    }
}

fn by_id(result_id: &str, anchor: Option<&str>) -> ContextRequest {
    ContextRequest::StageById {
        result_id: result_id.to_string(),
        anchor_id: anchor.map(str::to_string),
    }
}

#[tokio::test]
async fn a_self_loop_a_guard_redirect_and_a_terminal_arrival_each_record_the_actual_destination() {
    let f = gate_flow().await;
    // The entry is not a departure.
    assert!(f.results().await.is_empty());
    assert!(f.state().await.payload.get("current_result_id").is_none());

    f.reply("First note, fix A.\n/again").await.unwrap();
    f.reply("Second note.\n\n/again").await.unwrap();
    // The guard (max 1) sent the second `again` to `escalated`, not back to
    // the gate its `on:` edge names.
    assert_eq!(f.state().await.current_stage, "escalated");
    f.reply("go on then").await.unwrap();
    assert_eq!(f.state().await.current_stage, "finished");

    let results = f.results().await;
    assert_eq!(results.len(), 3, "{results:?}");
    let ids: std::collections::HashSet<_> = results.iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids.len(), 3, "a self-loop gets its own result IDs");
    let [first, second, third] = [
        result_of(&results[0]),
        result_of(&results[1]),
        result_of(&results[2]),
    ];

    assert_eq!(first.task_id, f.task_id);
    assert_eq!(first.source_stage, "gate");
    assert_eq!(first.source_kind, "human_gate");
    assert_eq!(first.outcome, "again");
    assert_eq!(first.destination_stage, "gate");
    assert!(first.capture_present);
    assert_eq!(first.capture, json!("First note, fix A."));
    assert_eq!(first.previous_result_id, None);

    assert_eq!(second.source_stage, "gate");
    assert_eq!(second.outcome, "again");
    assert_eq!(second.destination_stage, "escalated");
    assert_eq!(second.capture, json!("Second note."));
    assert_eq!(
        second.previous_result_id.as_deref(),
        Some(first.result_id.as_str())
    );

    // Terminal arrival: the destination of a departure, with no capture
    // because the stage declares none.
    assert_eq!(third.source_stage, "escalated");
    assert_eq!(third.outcome, "resumed");
    assert_eq!(third.destination_stage, "finished");
    assert!(!third.capture_present);
    assert_eq!(third.capture, Value::Null);
    assert_eq!(
        third.previous_result_id.as_deref(),
        Some(second.result_id.as_str())
    );

    let payload = f.state().await.payload;
    assert_eq!(payload["current_result_id"], json!(third.result_id));
    // The pointer is engine-owned, never a capture key.
    assert!(payload["stages"].get("current_result_id").is_none());
    for event in &results {
        assert_eq!(event.session_id, None);
    }
}

#[tokio::test]
async fn a_refused_or_discarded_outcome_records_nothing() {
    let f = gate_flow().await;
    f.reply("note\n/again").await.unwrap();
    let before = f.results().await;
    assert_eq!(before.len(), 1);
    let pointer = f.state().await.payload["current_result_id"].clone();
    assert_eq!(pointer, json!(before[0].id));

    // A reply without a marker is refused before anything is written.
    let err = f.reply("no marker line here").await.unwrap_err();
    assert!(
        matches!(err, SendMessageOrResumeError::ReplyNeedsMarker { .. }),
        "{err:?}"
    );
    // An outcome the stage has no edge for.
    let err = f
        .engine
        .advance(&f.task_id, &f.def, "bogus")
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::UnknownOutcome { .. }), "{err:?}");
    // A runner for a stage the task is no longer in.
    let err = f
        .engine
        .advance_from_stage(
            &f.task_id,
            &f.def,
            "again",
            Some("escalated"),
            Some(json!("late")),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::StageMovedOn { .. }), "{err:?}");
    // A cancelled task.
    f.engine.cancel_task(&f.task_id, false).await.unwrap();
    let err = f
        .engine
        .advance_from_stage(&f.task_id, &f.def, "again", Some("gate"), None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::TaskCancelled(_)), "{err:?}");

    assert_eq!(f.results().await, before);
    assert_eq!(f.state().await.payload["current_result_id"], pointer);
}

#[tokio::test]
async fn a_malformed_result_pointer_refuses_the_departure_but_null_is_a_fresh_start() {
    for bad in [json!(""), json!(7), json!([]), json!({}), json!(true)] {
        let f = gate_flow().await;
        f.reply("note\n/again").await.unwrap();
        let state = f.state().await;
        let mut payload = state.payload.clone();
        payload["current_result_id"] = bad.clone();
        workflow_state::update(
            &f.pool,
            &f.task_id,
            workflow_state::WorkflowStateUpdate {
                current_stage: state.current_stage.clone(),
                stage_kind: state.stage_kind.clone().unwrap(),
                loop_counters: state.loop_counters.clone(),
                payload: payload.clone(),
                enters_stage: false,
            },
        )
        .await
        .unwrap();
        let before = f.results().await;

        let outcome = f.engine.advance(&f.task_id, &f.def, "again").await;

        // Nothing moved and no result was invented as a new chain start.
        assert_eq!(f.results().await, before, "{bad}: no new result");
        let after = f.state().await;
        assert_eq!(after.current_stage, "gate", "{bad}");
        assert_eq!(after.payload, payload, "{bad}");
        assert!(
            matches!(outcome, Err(EngineError::MalformedResultPointer(_))),
            "{bad}: {outcome:?}"
        );
    }

    // JSON null is the pre-change boundary, exactly like an absent key.
    let f = gate_flow().await;
    let state = f.state().await;
    let mut payload = state.payload.clone();
    payload["current_result_id"] = Value::Null;
    workflow_state::update(
        &f.pool,
        &f.task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage,
            stage_kind: state.stage_kind.unwrap(),
            loop_counters: state.loop_counters,
            payload,
            enters_stage: false,
        },
    )
    .await
    .unwrap();
    f.reply("note\n/again").await.unwrap();
    let results = f.results().await;
    assert_eq!(results.len(), 1);
    assert_eq!(result_of(&results[0]).previous_result_id, None);
    assert_eq!(
        f.state().await.payload["current_result_id"],
        json!(results[0].id)
    );
}

#[tokio::test]
async fn pausing_resuming_and_an_in_place_retry_of_a_committed_self_loop_add_no_result() {
    let f = gate_flow().await;
    f.reply("note one\n/again").await.unwrap();
    let before = f.results().await;
    assert_eq!(before.len(), 1);
    let pointer = f.state().await.payload["current_result_id"].clone();

    f.engine.pause_task(&f.task_id, false).await.unwrap();
    f.engine.resume_task(&f.task_id).await.unwrap();
    assert_eq!(f.results().await, before);
    assert_eq!(f.state().await.payload["current_result_id"], pointer);

    // The self-loop already committed (gate -> gate). A stuck mark and a
    // retry re-enter the persisted gate; names alone cannot tell this apart
    // from a replay, so compare the result's identity and the pointer.
    let _ = f.engine.mark_stuck(&f.task_id, "interrupted", false).await;
    f.engine
        .retry_task(&f.task_id, RetryMode::Auto)
        .await
        .unwrap();
    let after = f.results().await;
    assert_eq!(
        after, before,
        "a retry must not append the source departure again"
    );
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(f.state().await.payload["current_result_id"], pointer);

    // The next real departure chains to the same, unchanged result.
    f.reply("approve now\n/approve").await.unwrap();
    let results = f.results().await;
    assert_eq!(results.len(), 2);
    assert_eq!(
        result_of(&results[1]).previous_result_id.as_deref(),
        Some(before[0].id.as_str())
    );
}

/// A shell stage whose output is captured by the production derivation.
async fn shell_result(command: &str, extra: &str) -> (SqlitePool, String, AppliedStageResult) {
    let pool = connect_in_memory().await.unwrap();
    let def = shell_def(command, extra);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;
    let results = stage_results(&pool, &task_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    let result = result_of(&results[0]);
    (pool, task_id, result)
}

#[tokio::test]
async fn every_capture_shape_is_recorded_as_the_transition_received_it() {
    // Text, trimmed as the capture always was.
    let (_, _, text) = shell_result(r#"printf '  hello\n'"#, "    capture: text").await;
    assert_eq!((text.capture_present, text.capture), (true, json!("hello")));
    assert_eq!(text.source_kind, "shell");
    assert_eq!(text.outcome, "done");
    assert_eq!(text.destination_stage, "finished");

    // Structured JSON.
    let (_, _, structured) =
        shell_result(r#"printf '{\"a\":[1,2],\"b\":null}'"#, "    capture: json").await;
    assert_eq!(
        (structured.capture_present, structured.capture),
        (true, json!({"a": [1, 2], "b": null}))
    );

    // A captured JSON null is a present capture.
    let (_, _, null) = shell_result("printf null", "    capture: json").await;
    assert_eq!((null.capture_present, null.capture), (true, Value::Null));

    // Invalid JSON is kept as text, as the capture always did.
    let (_, _, invalid) = shell_result("printf 'not json'", "    capture: json").await;
    assert_eq!(
        (invalid.capture_present, invalid.capture),
        (true, json!("not json"))
    );

    // No `capture:` at all: absent, which is not null.
    let (_, _, absent) = shell_result("printf 'ignored output'", "").await;
    assert_eq!(
        (absent.capture_present, absent.capture),
        (false, Value::Null)
    );

    // Over the 1 MiB limit the output is not captured, yet the transition
    // is legal and still recorded, explicitly without a capture.
    let (pool, task_id, oversized) =
        shell_result("yes x | head -c 1100000", "    capture: text").await;
    assert_eq!(
        (oversized.capture_present, oversized.capture),
        (false, Value::Null)
    );
    assert_eq!(oversized.outcome, "done");
    let _ = (pool, task_id);
}

#[tokio::test]
async fn a_discarded_capture_never_borrows_an_earlier_snapshot_for_the_same_stage() {
    let f = gate_flow().await;
    // An earlier capture for the stage, still in the payload.
    let state = f.state().await;
    let mut payload = state.payload.clone();
    payload["stages"] = json!({ "gate": "STALE EARLIER CAPTURE" });
    workflow_state::update(
        &f.pool,
        &f.task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage,
            stage_kind: state.stage_kind.unwrap(),
            loop_counters: state.loop_counters,
            payload,
            enters_stage: false,
        },
    )
    .await
    .unwrap();
    // 1.1 MB of reply is over the capture limit: discarded, yet `/approve`
    // still routes.
    let big = format!("{}\n/approve", "x".repeat(1_100_000));
    f.reply(&big).await.unwrap();
    assert_eq!(f.state().await.current_stage, "finished");

    let results = f.results().await;
    assert_eq!(results.len(), 1);
    let result = result_of(&results[0]);
    assert_eq!(result.outcome, "approved");
    assert!(!result.capture_present, "no stale replacement");
    assert_eq!(result.capture, Value::Null);
    let answer = found(ask(&f.pool, &f.task_id, by_name("gate", None, None)).await);
    assert!(!answer.capture_present);
    assert!(!serde_json::to_string(&answer).unwrap().contains("STALE"));
}

#[tokio::test]
async fn an_in_place_retry_of_a_stuck_stage_records_nothing_until_it_departs() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("ok");
    let yaml = format!(
        r#"name: sr-shell
stages:
  run:
    kind: shell
    command: "test -f {} && echo ran"
    capture: text
    on: {{ done: finished, error: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#,
        marker.display()
    );
    fs::write(dir.join("sr-shell.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    // The command fails and `error` leads to the gate: a legal departure.
    wait_until_stage(&pool, &task_id, "failed").await;
    let before = stage_results(&pool, &task_id).await;
    assert_eq!(before.len(), 1);
    assert_eq!(result_of(&before[0]).outcome, "error");

    let _ = engine
        .mark_stuck(&task_id, "operator wants a retry", false)
        .await;
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert_eq!(stage_results(&pool, &task_id).await, before);

    // Resuming the gate is the next genuine departure: exactly one more,
    // chained to the first.
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    let after = stage_results(&pool, &task_id).await;
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[0]);
    assert_eq!(
        result_of(&after[1]).previous_result_id.as_deref(),
        Some(before[0].id.as_str())
    );
}

#[tokio::test]
async fn a_failed_result_insert_leaves_the_old_state_and_pointer() {
    let f = gate_flow().await;
    f.reply("note\n/again").await.unwrap();
    let before_results = f.results().await;
    let before = f.state().await;

    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_results BEFORE INSERT ON events
         WHEN NEW.event_type = 'stage_result'
         BEGIN SELECT RAISE(ABORT, 'insert refused'); END",
    ))
    .execute(&f.pool)
    .await
    .unwrap();
    let outcome = f.engine.advance(&f.task_id, &f.def, "again").await;

    let after = f.state().await;
    assert_eq!(
        after.current_stage, before.current_stage,
        "stage stayed old after failed insert"
    );
    assert_eq!(
        after.payload, before.payload,
        "pointer and captures stayed old"
    );
    assert_eq!(after.loop_counters, before.loop_counters);
    assert_eq!(f.results().await, before_results);
    // The failure itself is reported, not swallowed.
    assert!(matches!(outcome, Err(EngineError::Db(_))), "{outcome:?}");
}

#[tokio::test]
async fn a_failed_state_update_leaves_no_orphan_result() {
    let f = gate_flow().await;
    let before = f.state().await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_state_moves BEFORE UPDATE ON workflow_state
         BEGIN SELECT RAISE(ABORT, 'update refused'); END",
    ))
    .execute(&f.pool)
    .await
    .unwrap();
    let outcome = f.engine.advance(&f.task_id, &f.def, "again").await;

    // The insert ran first and succeeded inside the transaction; it must not
    // have outlived the failed update.
    assert!(f.results().await.is_empty(), "no orphan applied result");
    let ask_answer = ask(&f.pool, &f.task_id, by_name("gate", None, None)).await;
    assert!(
        !matches!(ask_answer, ContextResponse::StageResult { .. }),
        "{ask_answer:?}"
    );
    assert_eq!(f.state().await, before);
    assert!(matches!(outcome, Err(EngineError::Db(_))), "{outcome:?}");
}

#[tokio::test]
async fn an_update_that_finds_no_row_rolls_the_result_back_too() {
    let f = gate_flow().await;
    // After the result is inserted, a trigger removes the workflow row, so
    // the update inside the same transaction finds nothing.
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER row_vanishes AFTER INSERT ON events
         WHEN NEW.event_type = 'stage_result'
         BEGIN DELETE FROM workflow_state WHERE task_id = NEW.task_id; END",
    ))
    .execute(&f.pool)
    .await
    .unwrap();
    let before = f.state().await;
    let err = f
        .engine
        .advance(&f.task_id, &f.def, "again")
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::NoWorkflowState), "{err:?}");
    assert!(f.results().await.is_empty());
    assert_eq!(
        f.state().await,
        before,
        "the deleted row came back with the rollback"
    );
}

/// An agent that drafts, a gate that sends changes back, and a final approval.
const LOOP_FLOW: &str = r#"name: sr-loop
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  draft:
    kind: agent_turn
    role: coder
    prompt_file: turn.md
    capture: text
    on: { done: review }
  review:
    kind: human_gate
    capture: text
    markers:
      - line: /changes
        then: changes_requested
      - line: /approve
        then: approved
    on: { changes_requested: draft, approved: finished }
  finished:
    kind: terminal
"#;

/// A serialized planner-report-shaped JSON document of exactly `len` bytes.
fn planner_report(len: usize) -> String {
    let build = |pad: usize| {
        json!({
            "summary": "Planner report",
            "steps": [{"id": 1, "title": "Split the module"}, {"id": 2, "title": "Add tests"}],
            "notes": "n".repeat(pad),
        })
        .to_string()
    };
    let base = build(0).len();
    let report = build(len - base);
    assert_eq!(report.len(), len);
    report
}

fn private_correction() -> String {
    let paragraph = "Please make exactly these three changes in the parser: first rename the \
        helper, second add the missing guard, third update its one test.";
    let text = format!(
        "{}Change nothing else.",
        format!("{paragraph}\n\n").repeat(20)
    );
    assert!(text.len() > 2048, "{}", text.len());
    text
}

#[tokio::test]
async fn repeated_stage_history_is_kept_in_full_and_survives_a_reopen() {
    let dir = tempdir();
    fs::write(dir.join("turn.md"), "do the thing").unwrap();
    fs::write(dir.join("sr-loop.yaml"), LOOP_FLOW).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(LOOP_FLOW, &dir).unwrap());
    let db_path = dir.join("history.db");
    let pool = crate::db::connect(&db_path).await.unwrap();
    let task_id = seed_task(&pool, &def.name).await;

    let first_report = planner_report(63_266);
    let binary = reply_binary(&dir, &first_report);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_for_results(&pool, &task_id, 1).await;
    wait_until_stage(&pool, &task_id, "review").await;

    // The human's private correction, longer than the 2,048-byte diagnostic
    // tails, ending "Change nothing else."
    let correction = private_correction();
    let second_report = "Second draft.\n\nIt addresses A, B and C only.".to_string();
    fs::write(dir.join("reply.txt"), &second_report).unwrap();
    engine
        .send_message_or_resume(&task_id, &format!("{correction}\n/changes"))
        .await
        .unwrap();
    wait_for_results(&pool, &task_id, 3).await;
    wait_until_stage(&pool, &task_id, "review").await;
    engine
        .send_message_or_resume(&task_id, "/approve\nLooks good now.")
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let events = stage_results(&pool, &task_id).await;
    assert_eq!(events.len(), 4);
    let r: Vec<AppliedStageResult> = events.iter().map(result_of).collect();
    let ids: std::collections::HashSet<_> = r.iter().map(|x| x.result_id.clone()).collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(
        r.iter()
            .map(|x| (
                x.source_stage.as_str(),
                x.outcome.as_str(),
                x.destination_stage.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            ("draft", "done", "review"),
            ("review", "changes_requested", "draft"),
            ("draft", "done", "review"),
            ("review", "approved", "finished"),
        ]
    );
    assert_eq!(
        r.iter().map(|x| x.source_kind.as_str()).collect::<Vec<_>>(),
        ["agent_turn", "human_gate", "agent_turn", "human_gate"]
    );
    assert_eq!(
        r[0].capture,
        json!(first_report),
        "the 63,266-byte report in full"
    );
    assert_eq!(
        r[1].capture,
        json!(correction),
        "the whole private correction"
    );
    assert_eq!(r[2].capture, json!(second_report));
    assert_eq!(r[3].capture, json!("Looks good now."));
    assert_eq!(r[0].previous_result_id, None);
    for i in 1..4 {
        assert_eq!(
            r[i].previous_result_id.as_deref(),
            Some(r[i - 1].result_id.as_str())
        );
    }
    assert_eq!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .unwrap()
            .payload["current_result_id"],
        json!(r[3].result_id)
    );

    // Timestamps deliberately contradict the chain: the oldest link looks
    // newest. Only the links may order the history.
    for (i, result) in r.iter().enumerate() {
        sqlx::query("UPDATE events SET created_at = ? WHERE id = ?")
            .bind(Utc::now() - chrono::Duration::hours(i as i64 + 1))
            .bind(&result.result_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Repeat every lookup on a fresh connection to the reopened file.
    for round in 0..2 {
        let pool = if round == 0 {
            pool.clone()
        } else {
            pool.close().await;
            crate::db::connect(&db_path).await.unwrap()
        };
        // Latest draft at the current anchor is the second draft; at the
        // anchor of the first correction it is still the first, full report.
        let latest = found(ask(&pool, &task_id, by_name("draft", None, None)).await);
        assert_eq!(latest.result_id, r[2].result_id, "round {round}");
        assert_eq!(latest.capture, json!(second_report));
        let old = found(
            ask(
                &pool,
                &task_id,
                by_name("draft", None, Some(&r[1].result_id)),
            )
            .await,
        );
        assert_eq!(old.result_id, r[0].result_id, "round {round}");
        assert_eq!(old.capture, json!(first_report));
        // A mix of stages and outcomes.
        let changes = found(
            ask(
                &pool,
                &task_id,
                by_name("review", Some("changes_requested"), None),
            )
            .await,
        );
        assert_eq!(changes.result_id, r[1].result_id);
        assert_eq!(changes.capture, json!(correction));
        let approved = found(ask(&pool, &task_id, by_name("review", Some("approved"), None)).await);
        assert_eq!(approved.result_id, r[3].result_id);
        // That approval is not at or before the old anchor.
        let anchored = found(
            ask(
                &pool,
                &task_id,
                by_name("review", None, Some(&r[2].result_id)),
            )
            .await,
        );
        assert_eq!(
            anchored.result_id, r[1].result_id,
            "the old review, not the newer approval"
        );
        match ask(
            &pool,
            &task_id,
            by_id(&r[3].result_id, Some(&r[2].result_id)),
        )
        .await
        {
            ContextResponse::Unavailable { reason, .. } => {
                assert_eq!(reason, UnavailableReason::NotAtOrBeforeAnchor)
            }
            other => panic!("{other:?}"),
        }
        // An exact ID is independent of the pointer, with the full value.
        let exact = found(ask(&pool, &task_id, by_id(&r[0].result_id, None)).await);
        // Everything but the timestamp, which the test rewrote above.
        assert_eq!(
            AppliedStageResult {
                created_at: r[0].created_at,
                ..exact
            },
            r[0].clone()
        );
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.payload["current_result_id"], json!(r[3].result_id));
    }
}

async fn wait_for_results(pool: &SqlitePool, task_id: &str, n: usize) {
    crate::test_support::wait_until(&format!("{n} stage result(s)"), || async {
        let found = stage_results(pool, task_id).await.len();
        if found >= n {
            Ok(())
        } else {
            Err(format!("{found} result(s)"))
        }
    })
    .await;
}

/// A watched gate whose watcher itself prints a request-changes verdict far
/// longer than the diagnostic tail. Nobody types a reply: the watcher's own
/// matched output is what departs the gate.
#[tokio::test]
async fn a_watched_gate_records_the_whole_watcher_output_not_the_diagnostic_tail() {
    let dir = tempdir();
    let correction = private_correction();
    fs::write(
        dir.join("verdict.txt"),
        format!("{correction}\nVERDICT: CHANGES_REQUESTED\n"),
    )
    .unwrap();
    let yaml = format!(
        r#"name: sr-watched
stages:
  review:
    kind: human_gate
    capture: text
    watch:
      command: 'cat {}'
      interval: 1h
      timeout: 30m
      outcomes:
        - match: 'VERDICT: CHANGES_REQUESTED'
          then: changes_requested
    on: {{ changes_requested: finished, timeout: finished }}
  finished:
    kind: terminal
"#,
        dir.join("verdict.txt").display()
    );
    fs::write(dir.join("sr-watched.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let pool = connect_in_memory().await.unwrap();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let results = stage_results(&pool, &task_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    let result = result_of(&results[0]);
    assert_eq!(result.source_kind, "human_gate");
    assert_eq!(result.outcome, "changes_requested");
    assert_eq!(result.previous_result_id, None);
    let full = format!("{correction}\nVERDICT: CHANGES_REQUESTED");
    assert!(full.len() > 2048);
    assert!(result.capture_present);
    assert_eq!(result.capture, json!(full), "the whole watcher output");

    // The diagnostic the watcher also wrote keeps only its 2,048-byte tail,
    // so the result cannot have been built from it.
    let tail = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::ShellOutput && e.payload["matched"].is_string())
        .expect("the matched attempt is recorded as a diagnostic");
    let tail = tail.payload["stdout_tail"].as_str().unwrap().to_string();
    let kept = tail
        .strip_prefix('…')
        .expect("a cut-off tail starts with an ellipsis");
    assert!(
        kept.len() <= 2048 && kept.len() < full.len(),
        "{}",
        kept.len()
    );
    assert!(full.ends_with(kept), "the tail is the end of the output");
    assert_ne!(json!(tail), result.capture);
    let answer = found(ask(&pool, &task_id, by_id(&result.result_id, None)).await);
    assert_eq!(answer.capture, json!(full));
}

#[tokio::test]
async fn a_destination_that_fails_to_start_keeps_the_one_committed_result_through_a_retry() {
    let dir = tempdir();
    fs::write(dir.join("turn.md"), "do the thing").unwrap();
    fs::write(dir.join("sr-loop.yaml"), LOOP_FLOW).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(LOOP_FLOW, &dir).unwrap());
    let pool = connect_in_memory().await.unwrap();
    let task_id = seed_task(&pool, &def.name).await;
    let binary = reply_binary(&dir, "first draft");
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_for_results(&pool, &task_id, 1).await;
    wait_until_stage(&pool, &task_id, "review").await;

    // From here on no session can be created: the destination fails to
    // start after the departure committed.
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_sessions BEFORE INSERT ON sessions
         BEGIN SELECT RAISE(ABORT, 'no sessions today'); END",
    ))
    .execute(&pool)
    .await
    .unwrap();
    let err = engine
        .send_message_or_resume(&task_id, "please redo\n/changes")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SendMessageOrResumeError::Advance(_)),
        "{err:?}"
    );

    let committed = stage_results(&pool, &task_id).await;
    assert_eq!(committed.len(), 2, "{committed:?}");
    let departure = result_of(&committed[1]);
    assert_eq!(departure.source_stage, "review");
    assert_eq!(departure.destination_stage, "draft");
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "draft");
    assert_eq!(
        state.payload["current_result_id"],
        json!(departure.result_id)
    );

    // Recovery re-enters the persisted destination; the review departure is
    // not appended again and keeps its identity.
    sqlx::query("DROP TRIGGER no_sessions")
        .execute(&pool)
        .await
        .unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_for_results(&pool, &task_id, 3).await;
    wait_until_stage(&pool, &task_id, "review").await;
    let after = stage_results(&pool, &task_id).await;
    let review_departures: Vec<_> = after
        .iter()
        .filter(|e| e.payload["source_stage"] == "review")
        .collect();
    assert_eq!(review_departures.len(), 1);
    assert_eq!(review_departures[0].id, committed[1].id);
    // The next departure (draft finishing again) chains to it.
    assert_eq!(
        result_of(&after[2]).previous_result_id.as_deref(),
        Some(committed[1].id.as_str())
    );
}

/// The loop flow on a file-backed database whose connections can be held in
/// the middle of a departure, parked at its review gate after one result.
struct HeldLoop {
    pool: SqlitePool,
    barrier: crate::test_support::DepartureBarrier,
    engine: Arc<WorkflowEngine>,
    task_id: String,
    _dir: TempDir,
}

async fn held_loop() -> HeldLoop {
    let dir = tempdir();
    fs::write(dir.join("turn.md"), "do the thing").unwrap();
    fs::write(dir.join("sr-loop.yaml"), LOOP_FLOW).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(LOOP_FLOW, &dir).unwrap());
    let (pool, barrier) = crate::test_support::DepartureBarrier::open(&dir.join("loop.db")).await;
    let task_id = seed_task(&pool, &def.name).await;
    let binary = reply_binary(&dir, "first draft");
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_for_results(&pool, &task_id, 1).await;
    wait_until_stage(&pool, &task_id, "review").await;
    HeldLoop {
        pool,
        barrier,
        engine,
        task_id,
        _dir: dir,
    }
}

impl HeldLoop {
    /// Everything a departure could have changed, as another process sees it.
    async fn seen_by_another_process(&self) -> (WorkflowState, Vec<Event>, usize) {
        let observer = self.barrier.observer().await;
        let seen = (
            state_of(&observer, &self.task_id).await,
            stage_results(&observer, &self.task_id).await,
            runs_for_stage(&observer, &self.task_id, "draft")
                .await
                .len(),
        );
        observer.close().await;
        seen
    }
}

#[tokio::test]
async fn a_departure_whose_commit_fails_leaves_the_old_state_and_starts_nothing() {
    let f = held_loop().await;
    let before = f.seen_by_another_process().await;
    assert_eq!(before.1.len(), 1);
    assert_eq!(before.0.current_stage, "review");

    crate::test_support::fail_commit_of_stage_results(&f.pool).await;
    let err = f
        .engine
        .send_message_or_resume(&f.task_id, "please redo\n/changes")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SendMessageOrResumeError::Advance(EngineError::Db(_))),
        "{err:?}"
    );

    // Both writes succeeded inside the transaction; the failed COMMIT took
    // both back, and the draft stage was never started.
    assert_eq!(
        f.seen_by_another_process().await,
        before,
        "old state and pointer, no orphan result, no destination session"
    );

    // The failure marked the task stuck, as any non-benign failure of a
    // reply does. Once the fault is gone a retry re-enters the persisted
    // gate without recording anything, and the same reply then departs
    // exactly once, linked to the earlier result.
    crate::test_support::stop_failing_commits(&f.pool).await;
    f.engine
        .retry_task(&f.task_id, RetryMode::Auto)
        .await
        .unwrap();
    assert_eq!(f.seen_by_another_process().await, before);
    f.engine
        .send_message_or_resume(&f.task_id, "please redo\n/changes")
        .await
        .unwrap();
    wait_for_results(&f.pool, &f.task_id, 3).await;
    let results = stage_results(&f.pool, &f.task_id).await;
    assert_eq!(results[1].payload["source_stage"], "review");
    assert_eq!(
        results[1].payload["previous_result_id"],
        json!(before.1[0].id)
    );
}

#[tokio::test]
async fn a_departure_cancelled_inside_its_transaction_leaves_nothing_and_can_be_redone() {
    let f = held_loop().await;
    let before = f.seen_by_another_process().await;

    // Hold the departure's INSERT inside its transaction: the result row
    // exists there, the state update and the COMMIT have not happened.
    f.barrier.arm(&f.pool).await;
    let reply = {
        let (engine, task_id) = (f.engine.clone(), f.task_id.clone());
        tokio::spawn(async move {
            engine
                .send_message_or_resume(&task_id, "please redo\n/changes")
                .await
        })
    };
    f.barrier.reached().await;
    reply.abort();
    assert!(reply.await.unwrap_err().is_cancelled());
    f.barrier.release(&f.pool).await;

    assert_eq!(
        f.seen_by_another_process().await,
        before,
        "a dropped departure leaves no orphan result and no move"
    );

    // The task lock and the connection are free again: the reply goes
    // through and records exactly one result.
    f.engine
        .send_message_or_resume(&f.task_id, "please redo\n/changes")
        .await
        .unwrap();
    wait_for_results(&f.pool, &f.task_id, 3).await;
    let results = stage_results(&f.pool, &f.task_id).await;
    assert_eq!(
        results
            .iter()
            .filter(|e| e.payload["source_stage"] == "review")
            .count(),
        1
    );
}

/// A task that reaches a terminal stage keeps its one result even when
/// closing the task afterwards fails, and neither a retry nor a restart-style
/// re-entry adds the departure again.
#[tokio::test]
async fn a_committed_departure_into_a_terminal_stage_is_not_replayed_by_recovery() {
    let f = gate_flow().await;
    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE TRIGGER no_closing BEFORE UPDATE ON tasks
         WHEN NEW.status = 'closed'
         BEGIN SELECT RAISE(ABORT, 'cannot close'); END",
    ))
    .execute(&f.pool)
    .await
    .unwrap();
    f.reply("fine\n/approve").await.unwrap();
    assert_eq!(f.state().await.current_stage, "finished");
    let results = f.results().await;
    assert_eq!(results.len(), 1);
    let pointer = f.state().await.payload["current_result_id"].clone();
    assert_eq!(pointer, json!(results[0].id));

    // Recovery in whatever way it is allowed: retry, with and without the
    // fault.
    let _ = f.engine.retry_task(&f.task_id, RetryMode::Auto).await;
    sqlx::query("DROP TRIGGER no_closing")
        .execute(&f.pool)
        .await
        .unwrap();
    let _ = f.engine.retry_task(&f.task_id, RetryMode::Auto).await;
    assert_eq!(f.results().await, results, "the same single result");
    assert_eq!(f.state().await.payload["current_result_id"], pointer);
    assert_eq!(f.state().await.current_stage, "finished");
}

/// A workflow may key an outcome with an empty string. The engine applies it
/// as written, so the result it records must be readable straight away.
#[tokio::test]
async fn an_outcome_keyed_by_an_empty_string_is_recorded_and_read_back() {
    let yaml = r#"name: sr-gate
stages:
  gate:
    kind: human_gate
    capture: text
    markers:
      - line: /go
        then: ""
    on: { "": finished }
  finished:
    kind: terminal
"#;
    let f = gate_flow_from(yaml, connect_in_memory().await.unwrap(), tempdir()).await;
    f.reply("proceed with A\n/go").await.unwrap();
    assert_eq!(f.state().await.current_stage, "finished");
    let results = f.results().await;
    assert_eq!(results.len(), 1);
    let answer = found(ask(&f.pool, &f.task_id, by_id(&results[0].id, None)).await);
    assert_eq!(answer.outcome, "");
    assert_eq!(answer.capture, json!("proceed with A"));
    let by_stage = found(ask(&f.pool, &f.task_id, by_name("gate", None, None)).await);
    assert_eq!(by_stage.result_id, results[0].id);
}
