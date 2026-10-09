use std::path::Path;
use std::time::Duration as StdDuration;

use serde_json::json;

use super::*;

#[test]
fn set_arrival_replaces_a_non_object_payload() {
    let mut payload = json!(null);
    set_arrival(&mut payload, "a", "b");
    assert_eq!(
        payload,
        json!({ "arrival": { "from": "a", "outcome": "b" } })
    );
}
use crate::adapter::{AgentAdapter, ClaudeAdapter, Registry};
use crate::db::{connect_in_memory, projects, tasks};
use crate::recording_adapter::{RecordedCall, RecordingAdapter};

fn fixture_binary(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

async fn seed_task(pool: &SqlitePool, workflow_def: &str) -> String {
    let project_id = projects::create(pool, "demo", None).await.unwrap().id;
    tasks::create(
        pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def,
            title: "T",
            config: json!({}),
            workflow_path: None,
            workflow_sha256: None,
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
        Registry::single(adapter),
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
        Registry::single(adapter),
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
    crate::test_support::wait_until(&format!("stage '{expected}' on task {task_id}"), || async {
        let state = workflow_state::get(pool, task_id).await.unwrap().unwrap();
        if state.current_stage == expected {
            Ok(())
        } else {
            Err(format!("stage '{}'", state.current_stage))
        }
    })
    .await
}

/// The last few event texts, for a wait's "last saw" message.
fn recent_texts(events: &[chocofactory_core::models::Event]) -> String {
    let texts: Vec<String> = events
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|e| {
            let text = e
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("<no text>");
            format!(
                "{}:{:?}",
                e.event_type,
                text.chars().take(80).collect::<String>()
            )
        })
        .collect();
    format!("{} events, last few: [{}]", events.len(), texts.join(", "))
}

/// Waits for `tasks.status`, which a terminal stage sets from inside
/// `enter_stage` — strictly after `current_stage` already names that
/// stage. Budgeted for a loaded parallel suite: these tests drive a real
/// python subprocess, and spawning one can take well over a second when
/// the whole workspace is running at once.
async fn wait_until_task_status(pool: &SqlitePool, task_id: &str, expected: &str) {
    crate::test_support::wait_until(
        &format!("status '{expected}' on task {task_id}"),
        || async {
            let status = tasks::get(pool, task_id).await.unwrap().unwrap().status;
            if status == expected {
                Ok(())
            } else {
                Err(format!("status {status:?}"))
            }
        },
    )
    .await
}

/// Polls `session_id`'s events for one whose `payload.text` equals
/// `text` (e.g. an assistant reply from the fake-claude fixture),
/// since event persistence happens on a spawned background task.
async fn wait_until_events_contain(pool: &SqlitePool, session_id: &str, text: &str) {
    crate::test_support::wait_until(
        &format!("an event with text {text:?} on session {session_id}"),
        || async {
            let events = crate::db::events::list_for_session(pool, session_id)
                .await
                .unwrap();
            if events
                .iter()
                .any(|e| e.payload.get("text").and_then(Value::as_str) == Some(text))
            {
                Ok(())
            } else {
                Err(recent_texts(&events))
            }
        },
    )
    .await
}

/// Same as `wait_until_events_contain`, but matches a *prefix* rather
/// than the whole string. `fake_claude_echo_args.py`'s reply always
/// carries `mcp_config=...` (issue #73: the tool is on every turn) —
/// an embedded, build-layout-dependent path to the `choco` binary —
/// so tests that only care about `model`/`system_prompt`/
/// `permission_mode` assert a prefix ending right before it instead of
/// pinning that path.
async fn wait_until_events_contain_prefix(pool: &SqlitePool, session_id: &str, prefix: &str) {
    crate::test_support::wait_until(
        &format!("an event with text starting with {prefix:?} on session {session_id}"),
        || async {
            let events = crate::db::events::list_for_session(pool, session_id)
                .await
                .unwrap();
            if events.iter().any(|e| {
                e.payload
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(prefix))
            }) {
                Ok(())
            } else {
                Err(recent_texts(&events))
            }
        },
    )
    .await
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

// ---- arrival (#112) ----

/// A single-transition workflow whose entry stage is `from_stage`,
/// moving to `revising` on `outcome` — for pinning what
/// `advance_from_stage` writes into `payload.arrival` on one specific
/// path, independent of the real `coding-task.yaml`'s shape.
fn arrival_path_def(from_stage: &str, outcome: &str) -> Arc<WorkflowDefinition> {
    let yaml = format!(
        r#"
name: arrival-path
stages:
  {from_stage}:
    kind: human_gate
    on: {{ {outcome}: revising }}
  revising:
    kind: human_gate
    on: {{ done: finished }}
  finished:
    kind: terminal
"#
    );
    Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap())
}

#[tokio::test]
async fn arrival_records_the_transition_from_internal_review() {
    let pool = connect_in_memory().await.unwrap();
    let def = arrival_path_def("internal_review", "changes_requested");
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "revising");
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "internal_review", "outcome": "changes_requested" })
    );
}

#[tokio::test]
async fn arrival_records_the_transition_from_awaiting_human_review() {
    let pool = connect_in_memory().await.unwrap();
    let def = arrival_path_def("awaiting_human_review", "changes_requested");
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "revising");
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "awaiting_human_review", "outcome": "changes_requested" })
    );
}

#[tokio::test]
async fn arrival_records_the_transition_from_checks_polling() {
    let pool = connect_in_memory().await.unwrap();
    let def = arrival_path_def("checks_polling", "red");
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine.advance(&task_id, &def, "red").await.unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "revising");
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "checks_polling", "outcome": "red" })
    );
}

/// Unlike the three tests above, this drives the real
/// `send_message_or_resume` relay (#59) rather than calling `advance`
/// directly — the actual caller a human's reply to an escalated task
/// goes through — to prove the "resumed" arrival is recorded on that
/// path too, not just when a test calls `advance_from_stage` by hand.
#[tokio::test]
async fn arrival_records_the_transition_from_escalate_to_human_via_the_real_resume_path() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    // `send_message_or_resume` loads the task's workflow by name off
    // disk (`load_task_workflow`), unlike `advance`, which is handed
    // the definition directly — so this needs a real file, not just
    // the parsed `Arc<WorkflowDefinition>` the other arrival tests use.
    let yaml = r#"
name: arrival-path
stages:
  escalate_to_human:
    kind: human_gate
    on: { resumed: revising }
  revising:
    kind: human_gate
    on: { done: finished }
  finished:
    kind: terminal
"#;
    std::fs::write(dir.join("arrival-path.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine
        .send_message_or_resume(&task_id, "go ahead and fix it this way")
        .await
        .unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "revising");
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "escalate_to_human", "outcome": "resumed" })
    );
}

#[tokio::test]
async fn a_fresh_tasks_entry_stage_has_an_empty_arrival_with_no_unresolved_note() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    fs::write(
        dir.join("coder-turn.md"),
        "from=[{{ arrival.from }}] outcome=[{{ arrival.outcome }}]",
    )
    .unwrap();
    let yaml = r#"
name: arrival-entry
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

    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "coding").await;

    // The turn ran with both fields substituted as empty strings — a
    // present-but-empty value, distinct from a genuinely missing one.
    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
    let run = runs.iter().find(|r| r.stage == "coding").unwrap();
    wait_until_events_contain(&pool, &run.id, "echo:from=[] outcome=[]").await;

    assert!(
        events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .all(|e| e.event_type != EventType::TemplateUnresolved),
        "an empty (but present) arrival is a resolved value, not a missing one (#60)"
    );

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "", "outcome": "" })
    );
}

/// #112: a task created before this change has no `arrival` key in its
/// payload at all until its next `advance_from_stage` — simulated here
/// by seeding `workflow_state` directly (bypassing `start_task`, which
/// now always seeds it) and re-entering the stage. This must render
/// the same way a pre-existing missing `task` key already does: empty,
/// with the ordinary #60 note, never a hard failure.
#[tokio::test]
async fn a_pre_upgrade_payload_with_no_arrival_key_renders_empty_and_is_noted() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: arrival-legacy
stages:
  coding:
    kind: shell
    command: "test -z \"{{ arrival.from }}\""
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");

    let state = workflow_state::create(&pool, &task_id, "coding", "agent_turn", json!({}))
        .await
        .unwrap();
    engine
        .enter_stage(&task_id, &def, "coding", None, None, &state.payload, None)
        .await
        .unwrap();

    wait_until_stage(&pool, &task_id, "finished").await;

    let note = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::TemplateUnresolved)
        .unwrap_or_else(|| panic!("expected a template_unresolved note on the timeline"));
    assert_eq!(note.payload["stage"], json!("coding"));
    assert_eq!(note.payload["placeholders"], json!(["{{ arrival.from }}"]));
}

/// #112, design point 4: `retry_task` re-enters the current stage
/// directly (`enter_stage`, not `advance_from_stage`), so a retried
/// turn must keep the arrival that originally brought it there rather
/// than recording `revising --[retry]--> revising`.
#[tokio::test]
async fn retrying_a_stuck_stage_leaves_its_arrival_unchanged() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created, so `revising` always fails
    let yaml = format!(
        r#"
name: retry-keeps-arrival
stages:
  checks_polling:
    kind: human_gate
    on: {{ red: revising }}
  revising:
    kind: shell
    command: "test -f {}"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
        marker.display()
    );
    std::fs::write(dir.join("retry-keeps-arrival.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    // `retry_task` loads the task's workflow by name off disk
    // (`load_task_workflow`), so this needs a real file in the engine's
    // workflows directory, unlike `advance`, which takes the
    // definition directly.
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    engine.start_task(&task_id, &def, None).await.unwrap();
    engine.advance(&task_id, &def, "red").await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let before = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(before.current_stage, "revising");
    let expected_arrival = json!({ "from": "checks_polling", "outcome": "red" });
    assert_eq!(before.payload["arrival"], expected_arrival);
    assert_eq!(before.payload["finished_stages"], json!(["checks_polling"]));

    // The marker is still missing, so the retried command fails again
    // and the task lands back on `stuck` without ever going through
    // `advance_from_stage` — the case this test exists to pin.
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let after = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(after.current_stage, "revising");
    assert_eq!(after.payload["arrival"], expected_arrival);
    assert_eq!(
        after.payload["finished_stages"],
        json!(["checks_polling"]),
        "a retry re-enters the stage without finishing it"
    );
}

/// #112, design point 1: a `loop_guard` reroute still records the
/// stage actually left and the outcome that fired the reroute —
/// `internal_review`/`changes_requested` — not a synthetic value for
/// having been rerouted.
#[tokio::test]
async fn a_loop_guard_reroute_records_the_stage_and_outcome_that_tripped_it() {
    let pool = connect_in_memory().await.unwrap();
    let def = coder_reviewer_guard_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    // 3 round trips are allowed by the guard; the 4th reroutes to
    // escalate_to_human instead of back to coding.
    for _ in 0..4 {
        engine.advance(&task_id, &def, "resumed").await.unwrap();
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
    }

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate_to_human");
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "internal_review", "outcome": "changes_requested" })
    );
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

    // `gate` is a human_gate, so no session exists to attribute this
    // to — the case the old schema could not store at all.
    let recorded = events::list_for_task(&pool, &task_id).await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].session_id, None);
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
async fn loop_guard_count_does_not_reset_when_the_guarded_stage_is_entered_from_elsewhere() {
    let pool = connect_in_memory().await.unwrap();
    let def = two_paths_into_guarded_stage_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    // start -> review, then one round trip through coding back into
    // review — a different prior stage than last time review was
    // entered. Which stage it came from does not matter: the count only
    // resets when review resolves with another outcome or the task lands
    // on the guard's `then:` (here, "escalate"), and neither happened.
    engine.advance(&task_id, &def, "go").await.unwrap();
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    engine.advance(&task_id, &def, "resumed").await.unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "review");
    assert_eq!(state.loop_counters["review"]["count"], json!(1));

    // A second transition through the guarded outcome carries the
    // count forward instead of restarting it.
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        state.loop_counters["review"],
        json!({ "count": 2 }),
        "no entered_from: the counter's only shape is {{ count }}"
    );
}

/// Mirrors `coding-task.yaml`'s actual shape (#106): the guarded stage
/// (`review`) is entered once from `coding` and every later time from
/// `revising`, so a reset keyed on "did the prior stage change" fires
/// on the very first round trip and forgets it.
fn coding_revising_review_escalate_def() -> Arc<WorkflowDefinition> {
    let yaml = r#"
name: coding-revising-review
stages:
  coding:
    kind: human_gate
    on: { resumed: review }
  revising:
    kind: human_gate
    on: { resumed: review }
  review:
    kind: human_gate
    on:
      approved: done
      changes_requested: revising
    loop_guard: { on: changes_requested, max: 3, then: escalate }
  escalate:
    kind: human_gate
    on: { resumed: revising }
  done:
    kind: terminal
"#;
    Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
}

/// #106, item 1 (must fail on main): the guarded stage is entered from
/// a different prior stage on its very first return (`coding` once,
/// then `revising` every time after), which is exactly the shape that
/// made `internal_review`'s reset condition fire on the 1st rejection.
/// With the old entry-based reset this escalates on the 5th CR, not
/// the 4th.
#[tokio::test]
async fn loop_guard_escalates_on_the_fourth_cr_even_though_first_entered_from_a_different_stage() {
    let pool = connect_in_memory().await.unwrap();
    let def = coding_revising_review_escalate_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine.advance(&task_id, &def, "resumed").await.unwrap(); // coding -> review

    // CRs 1-3 leave the task in revising (guard allows 3 laps).
    for _ in 0..3 {
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "revising");
        engine.advance(&task_id, &def, "resumed").await.unwrap(); // revising -> review
    }

    // The 4th CR reroutes to escalate.
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
}

/// #106, item 3: `awaiting_human_review`'s shape — the guarded stage is
/// entered from exactly one other stage every time — never tripped the
/// old bug, and must keep working under the new rule too.
fn human_only_entered_from_ci_def() -> Arc<WorkflowDefinition> {
    let yaml = r#"
name: human-only-from-ci
stages:
  ci:
    kind: human_gate
    on: { green: human }
  human:
    kind: human_gate
    on:
      approved: done
      changes_requested: revising
    loop_guard: { on: changes_requested, max: 3, then: escalate }
  revising:
    kind: human_gate
    on: { resumed: ci }
  escalate:
    kind: human_gate
    on: { resumed: revising }
  done:
    kind: terminal
"#;
    Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
}

#[tokio::test]
async fn loop_guard_still_trips_when_the_guarded_stage_has_only_one_entry_stage() {
    let pool = connect_in_memory().await.unwrap();
    let def = human_only_entered_from_ci_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine.advance(&task_id, &def, "green").await.unwrap(); // ci -> human

    for _ in 0..3 {
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap(); // human -> revising
        engine.advance(&task_id, &def, "resumed").await.unwrap(); // revising -> ci
        engine.advance(&task_id, &def, "green").await.unwrap(); // ci -> human
    }

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "human");

    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
}

/// #106, item 6: the escalation stage can also be reached from a cause
/// unrelated to the guard (here, `revising`'s `error` outcome). Resuming
/// from there must still hand the guard a fresh budget: arriving at
/// `escalate` clears `review`'s counter. An approval also resets the
/// count now, so this test reaches `escalate` by a route that doesn't
/// pass through one; otherwise it would no longer prove the reset on
/// arrival.
#[tokio::test]
async fn a_guard_gets_a_fresh_budget_after_resuming_from_an_escalation_with_another_cause() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: escalate-path
stages:
  coding:
    kind: human_gate
    on: { resumed: review }
  revising:
    kind: human_gate
    on: { resumed: review, error: escalate }
  review:
    kind: human_gate
    on:
      approved: pr
      changes_requested: revising
    loop_guard: { on: changes_requested, max: 3, then: escalate }
  pr:
    kind: human_gate
    on: { done: finished, error: escalate }
  escalate:
    kind: human_gate
    on: { resumed: revising }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine.advance(&task_id, &def, "resumed").await.unwrap(); // coding -> review

    // 3 CRs, staying under the guard; the task ends in `revising`.
    for i in 0..3 {
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap(); // review -> revising
        if i < 2 {
            engine.advance(&task_id, &def, "resumed").await.unwrap(); // revising -> review
        }
    }

    // Escalate from `revising` (review's count still 3), not through an
    // approval, which would reset the count by itself.
    engine.advance(&task_id, &def, "error").await.unwrap(); // revising -> escalate
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
    assert!(state.loop_counters.get("review").is_none());

    engine.advance(&task_id, &def, "resumed").await.unwrap(); // escalate -> revising
    engine.advance(&task_id, &def, "resumed").await.unwrap(); // revising -> review

    // 3 more CRs stay in the loop...
    for _ in 0..3 {
        engine
            .advance(&task_id, &def, "changes_requested")
            .await
            .unwrap();
        let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(state.current_stage, "revising");
        engine.advance(&task_id, &def, "resumed").await.unwrap();
    }
    // ...and the 4th escalates.
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
}

/// #106, item 7: two guards sharing the same `then:` both clear on
/// arrival there; a third guard with a different `then:` is
/// unaffected.
#[tokio::test]
async fn tripping_one_guard_clears_every_guard_sharing_its_then_but_not_others() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: three-guards
stages:
  g1:
    kind: human_gate
    on: { loop: g2, next: g2 }
    loop_guard: { on: loop, max: 1, then: escalate }
  g2:
    kind: human_gate
    on: { loop: g3, next: g3 }
    loop_guard: { on: loop, max: 5, then: escalate }
  g3:
    kind: human_gate
    on: { loop: g1, next: g1 }
    loop_guard: { on: loop, max: 5, then: elsewhere }
  escalate:
    kind: terminal
  elsewhere:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    // Each stage leaves through its guarded outcome, so no count resets.
    engine.advance(&task_id, &def, "loop").await.unwrap(); // g1, count 1 -> g2
    engine.advance(&task_id, &def, "loop").await.unwrap(); // g2, count 1 -> g3
    engine.advance(&task_id, &def, "loop").await.unwrap(); // g3, count 1 -> g1

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        state.loop_counters,
        json!({ "g1": {"count": 1}, "g2": {"count": 1}, "g3": {"count": 1} })
    );

    // Trips g1's guard (2nd "loop" > max 1).
    engine.advance(&task_id, &def, "loop").await.unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
    assert_eq!(state.loop_counters, json!({ "g3": {"count": 1} }));
}

/// #106, item 8: arriving at a stage that is nobody's `then:` clears
/// nothing, even though it's a real, non-trivial transition.
#[test]
fn clear_guards_escaping_to_a_stage_that_is_no_guards_then_is_a_no_op() {
    let def = coding_revising_review_escalate_def();
    let mut loop_counters = json!({ "review": { "count": 2 } });
    clear_guards_escaping_to(&mut loop_counters, &def, "review");
    assert_eq!(loop_counters, json!({ "review": { "count": 2 } }));
}

/// #106, item 11: a pre-#106 `{ entered_from, count }` entry keeps its
/// count the first time it's bumped after the upgrade, just losing the
/// now-meaningless `entered_from`.
#[tokio::test]
async fn a_pre_106_entry_keeps_its_count_and_drops_entered_from_on_the_next_bump() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: pre-106-shape
stages:
  review:
    kind: human_gate
    on: { changes_requested: review, approved: done }
    loop_guard: { on: changes_requested, max: 3, then: escalate }
  escalate:
    kind: terminal
  done:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    workflow_state::update(
        &pool,
        &task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage,
            stage_kind: "agent_turn".to_string(),
            loop_counters: json!({ "review": { "entered_from": "coding", "count": 2 } }),
            payload: state.payload,
            enters_stage: false,
        },
    )
    .await
    .unwrap();

    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "review");
    assert_eq!(state.loop_counters, json!({ "review": { "count": 3 } }));

    // The next one escalates: 2 stored + 1 bump above = 3 laps used;
    // this 4th bump exceeds max: 3.
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "escalate");
}

/// #106, item 10: `retry_task` never touches `loop_counters`, even
/// when the stuck stage happens to be some other guard's `then:` and
/// another guard's count is non-zero at the time.
#[tokio::test]
async fn retry_task_leaves_loop_counters_untouched_even_at_a_guards_then_stage() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created -> escalate stays stuck
    let yaml = format!(
        r#"
name: retry-at-guard-then
stages:
  other:
    kind: human_gate
    on: {{ loop: review, next: review }}
    loop_guard: {{ on: loop, max: 5, then: other_escalate }}
  review:
    kind: human_gate
    on:
      changes_requested: coding
      approved: done
    loop_guard: {{ on: changes_requested, max: 1, then: escalate }}
  coding:
    kind: human_gate
    on: {{ resumed: review }}
  escalate:
    kind: shell
    command: "test -f {}"
    on: {{ done: coding }}
  other_escalate:
    kind: terminal
  done:
    kind: terminal
"#,
        marker.display()
    );
    std::fs::write(dir.join("retry-at-guard-then.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap(); // -> other

    engine.advance(&task_id, &def, "loop").await.unwrap(); // other, count 1 -> review
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap(); // review, count 1 (not tripped) -> coding
    engine.advance(&task_id, &def, "resumed").await.unwrap(); // -> review
    engine
        .advance(&task_id, &def, "changes_requested")
        .await
        .unwrap(); // review, count 2 > 1 -> trips, escalate clears review

    wait_until_task_status(&pool, &task_id, "stuck").await;
    let before = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(before.current_stage, "escalate");
    assert_eq!(before.loop_counters, json!({ "other": { "count": 1 } }));

    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let after = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        after.loop_counters, before.loop_counters,
        "retry_task must not touch loop_counters, byte-for-byte"
    );
    assert_eq!(after.current_stage, "escalate");
}

/// #164: a transition stamps `stage_entered_at`; a retry of a stuck task
/// re-runs the stage without re-entering it, so it stays put.
#[tokio::test]
async fn transition_stamps_stage_entered_at_and_retry_leaves_it() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created -> `run` stays stuck
    let yaml = format!(
        r#"
name: stamp
stages:
  gate:
    kind: human_gate
    on: {{ resumed: run }}
  run:
    kind: shell
    command: "test -f {}"
    on: {{ done: done }}
  done:
    kind: terminal
"#,
        marker.display()
    );
    std::fs::write(dir.join("stamp.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    let at_gate = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(at_gate.stage_entered_at, Some(at_gate.updated_at));

    tokio::time::sleep(Duration::from_millis(5)).await;
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let stuck = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(stuck.current_stage, "run");
    assert!(stuck.stage_entered_at > at_gate.stage_entered_at);

    tokio::time::sleep(Duration::from_millis(5)).await;
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let retried = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(retried.stage_entered_at, stuck.stage_entered_at);
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

    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
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

/// Regression test for #70. `fake_claude_oneshot.py` above self-exits
/// after one turn, which already worked before this fix — the bug is
/// specifically that the real `claude --input-format stream-json` CLI
/// never exits on its own. `fake_claude.py` reproduces that shape (it
/// loops on stdin until EOF), so this hangs — `wait_until_stage` times
/// out — without the fix, and passes once a single-shot turn completes
/// on the CLI's own `result` line instead of on process exit.
#[tokio::test]
async fn a_single_shot_agent_turn_auto_advances_against_a_cli_that_never_exits_on_its_own() {
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
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

    engine.start_task(&task_id, &def, None).await.unwrap();

    wait_until_stage(&pool, &task_id, "finished").await;
    wait_until_task_status(&pool, &task_id, "closed").await;

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

    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    crate::test_support::wait_until(&format!("session {} to exit", runs[0].id), || async {
        let status = sessions::get(&pool, &runs[0].id)
            .await
            .unwrap()
            .unwrap()
            .status;
        if status == SessionStatus::Exited {
            Ok(())
        } else {
            Err(format!("status {status:?}"))
        }
    })
    .await;

    // Give the watcher a moment it would need if it had incorrectly
    // decided to auto-advance, then confirm it didn't — and that it
    // marked the task stuck instead (X-4, issue #61) rather than
    // leaving it looking healthy.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "coding");
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("coding")),
        "{:?}",
        task.stuck_reason
    );
}

#[tokio::test]
async fn a_turn_reaped_by_the_idle_timeout_does_not_auto_advance() {
    // Regression test for the ambiguity the review on PR #35 flagged:
    // both a completed turn and a reaper-force-closed turn land the
    // session on `Idle`, so the watcher must consult `end_reason`
    // rather than treating every `Idle` as "done". The session is
    // seeded directly as already `Idle`/`reaped` so the watcher's
    // very first poll observes the condition deterministically,
    // rather than racing a real subprocess to get there first.
    let pool = connect_in_memory().await.unwrap();
    let def = human_gate_chain_def();
    let task_id = seed_task(&pool, &def.name).await;
    workflow_state::create(&pool, &task_id, "gate", "human_gate", json!({}))
        .await
        .unwrap();
    let session = sessions::create(
        &pool,
        sessions::NewSession {
            task_id: &task_id,
            stage: "gate",
            role: "chat",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    sessions::update_status(
        &pool,
        &session.id,
        SessionStatus::Idle,
        None,
        Some(SessionEndReason::Reaped),
    )
    .await
    .unwrap();

    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.spawn_turn_watcher(
        task_id.clone(),
        Arc::clone(&def),
        "chatting".to_string(),
        None,
        session.id.clone(),
    );

    // Also marks the task stuck (X-4, issue #61; review round 2): the
    // idle reaper closing a turn mid-flight is exactly the kind of
    // giving-up this feature exists to surface, not just log.
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "gate");
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("chatting") && r.contains("idle reaper")),
        "{:?}",
        task.stuck_reason
    );
}

#[tokio::test]
async fn a_failed_session_start_marks_the_session_exited_instead_of_wedging_it() {
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

    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, SessionStatus::Exited);
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
    crate::test_support::wait_until(
        &format!("a shell_output event on task {task_id}"),
        || async {
            let all = events::list_for_task(pool, task_id).await.unwrap();
            match all.iter().find(|e| e.event_type == EventType::ShellOutput) {
                Some(event) => Ok(event.payload.clone()),
                None => Err(recent_texts(&all)),
            }
        },
    )
    .await
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

    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = tasks::create(
        &pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: &def.name,
            title: "T",
            config: json!({ "cwd": dir.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
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

    // `task` and `arrival` are the only payload here — the stage
    // itself writes nothing, since it declares no `capture:` (#112:
    // `arrival` now always accompanies the transition into `finished`).
    assert_eq!(
        payload_of(&pool, &task_id).await,
        json!({
            "task": {"input": null, "title": "T"},
            "arrival": {"from": "run", "outcome": "done"},
            "finished_stages": ["run"],
        })
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
/// stage opens none — so it must carry a null `session_id` and still
/// appear on the task's timeline.
#[tokio::test]
async fn the_shell_event_is_task_scoped_with_no_session() {
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
    assert_eq!(event.session_id, None);
    assert_eq!(event.task_id, task_id);
    assert!(
        sessions::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// `error` is deliberately optional in a shell stage's `on:` map, so a
/// failed command with nowhere to go parks the task where it is rather
/// than crashing or inventing a transition — and marks it `stuck`
/// (X-4, issue #61) so a human can find and retry it.
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
    // ...but there was nowhere to go, so the task stays put and is
    // marked stuck, naming the stage.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "run");
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("run")),
        "{:?}",
        task.stuck_reason
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

    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let def = shell_def("ls", "    capture: text");
    let task_id = tasks::create(
        &pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: &def.name,
            title: "T",
            config: json!({ "cwd": dir.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
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
        .advance_from_stage(
            &task_id,
            &def,
            "done",
            Some("run"),
            Some(json!("late")),
            false,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::StageMovedOn { expected, actual }
                if expected == "run" && actual == "finished"),
        "got {err:?}"
    );
    // Neither the transition nor the capture was applied — `arrival`
    // still reflects the real `run --[done]--> finished` transition
    // from `start_task`'s advance, not the rejected late outcome.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "finished");
    assert_eq!(
        state.payload,
        json!({
            "task": {"input": null, "title": "T"},
            "arrival": {"from": "run", "outcome": "done"},
            "finished_stages": ["run"],
        })
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
    let (captured, note) = derive_capture(Some(Capture::Text), &at_limit, "task", "run", "stdout");
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

/// `resolve_task_workflow`'s `is_valid_workflow_name` check (issue #88
/// review, F1): without a direct test, deleting the check (or moving it
/// after the path joins) would still pass the rest of the suite while
/// reopening path traversal at `POST /tasks`, since the name is joined
/// onto a caller-controlled `project.repo_path`.
#[test]
fn resolve_task_workflow_only_accepts_a_safe_allowlisted_name() {
    let workflows_dir = tempdir();
    std::fs::write(workflows_dir.join("chat.yaml"), "irrelevant").unwrap();
    let repo = tempdir();
    write_repo_workflow(&repo, "chat", "irrelevant");
    let project = Project {
        id: "p1".to_string(),
        name: "demo".to_string(),
        repo_path: Some(repo.display().to_string()),
        created_at: chrono::Utc::now(),
    };

    assert!(resolve_task_workflow(&workflows_dir, &project, "chat").is_ok());
    assert!(matches!(
        resolve_task_workflow(&workflows_dir, &project, "").unwrap_err(),
        ResolveError::InvalidName(_)
    ));
    assert!(matches!(
        resolve_task_workflow(&workflows_dir, &project, "../etc/passwd").unwrap_err(),
        ResolveError::InvalidName(_)
    ));
    assert!(matches!(
        resolve_task_workflow(&workflows_dir, &project, "chat/../../etc").unwrap_err(),
        ResolveError::InvalidName(_)
    ));
    assert!(matches!(
        resolve_task_workflow(&workflows_dir, &project, "does-not-exist").unwrap_err(),
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(
            &project_id,
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
    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].stage, "chatting");
}

#[tokio::test]
async fn create_task_with_an_unknown_workflow_name_errors() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

    let err = engine
        .create_task(&project_id, "ghost", "t", "hi", json!({}))
        .await
        .unwrap_err();
    let CreateTaskError::Resolve(ResolveError::NotFound(message)) = &err else {
        panic!("expected Resolve(NotFound), got {err:?}");
    };
    assert!(message.contains("ghost"), "{message}");
}

/// `create_task` itself must reject an invalid workflow name before it
/// ever tries to search for it (issue #88 review, F1) — a unit test on
/// `resolve_task_workflow` alone wouldn't catch a regression where
/// `create_task` stopped calling it, or called it after building the
/// repo/global candidate paths instead of before.
#[tokio::test]
async fn create_task_with_an_invalid_workflow_name_is_rejected_before_any_lookup() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

    let err = engine
        .create_task(&project_id, "../etc/passwd", "t", "hi", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CreateTaskError::Resolve(ResolveError::InvalidName(_))
    ));
}

#[tokio::test]
async fn create_task_with_a_nonexistent_project_id_is_a_reported_error_not_a_raw_fk_failure() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &workflows_dir);

    let err = engine
        .create_task("no-such-project", "chat", "t", "hi", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CreateTaskError::NoSuchProject(id) if id == "no-such-project"
    ));
}

// ---- project workflows: repo path resolution (issue #88) ----

/// Writes `<dir>/.chocofactory/workflows/<name>.yaml`, creating both
/// directories.
fn write_repo_workflow(repo: &Path, name: &str, yaml: &str) -> PathBuf {
    let workflows_dir = repo.join(".chocofactory").join("workflows");
    std::fs::create_dir_all(&workflows_dir).unwrap();
    let path = workflows_dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

fn one_role_workflow_yaml(name: &str, model: &str) -> String {
    format!(
        r#"
name: {name}
roles:
  chat:
    cli: claude
    model: {model}
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {{}}
"#
    )
}

/// A project with a repo that has `.chocofactory/workflows/x.yaml`, and
/// a *different* global `x.yaml` (a different role model, so the two
/// files are byte-distinct): `create_task` must use the repo file, and
/// record its canonical path and its own SHA-256 — not the global
/// file's.
#[tokio::test]
async fn create_task_prefers_the_projects_repo_workflow_over_the_builtin_one() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    std::fs::write(
        global_dir.join("x.yaml"),
        one_role_workflow_yaml("x", "sonnet"),
    )
    .unwrap();
    let repo_path = write_repo_workflow(&repo_dir, "x", &one_role_workflow_yaml("x", "opus"));

    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    let task = engine
        .create_task(&project.id, "x", "t", "hi", json!({}))
        .await
        .unwrap();

    let canonical_repo_path = std::fs::canonicalize(&repo_path).unwrap();
    assert_eq!(
        task.workflow_path.as_deref(),
        Some(canonical_repo_path.to_string_lossy().as_ref())
    );
    let repo_bytes = std::fs::read(&repo_path).unwrap();
    assert_eq!(
        task.workflow_sha256.as_deref(),
        Some(sha256_hex(&repo_bytes).as_str())
    );
}

/// The repo has no `x.yaml`, so the built-in is used and recorded as
/// `builtin:<name>@<VERSION>` with the SHA of the built-in YAML.
#[tokio::test]
async fn create_task_falls_back_to_the_builtin_workflow_when_the_repo_lacks_it() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    let repo_dir = tempdir();
    let yaml = one_role_workflow_yaml("x", "sonnet");
    std::fs::write(builtin_dir.join("x.yaml"), &yaml).unwrap();
    // The repo exists but has no `.chocofactory/workflows/x.yaml`.

    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    );

    let task = engine
        .create_task(&project.id, "x", "t", "hi", json!({}))
        .await
        .unwrap();

    assert_eq!(
        task.workflow_path.as_deref(),
        Some(format!("builtin:x@{}", chocofactory_core::version::VERSION).as_str())
    );
    assert_eq!(
        task.workflow_sha256.as_deref(),
        Some(sha256_hex(yaml.as_bytes()).as_str())
    );
    assert_eq!(task.workflow_def, "x");
}

/// Neither the repo nor the built-ins have the workflow: the message
/// names the repo path tried and lists the built-in names, sorted.
#[tokio::test]
async fn create_task_when_neither_repo_nor_builtin_has_it_names_the_repo_path_and_builtins() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    std::fs::write(builtin_dir.join("zeta.yaml"), "x").unwrap();
    std::fs::write(builtin_dir.join("alpha.yaml"), "x").unwrap();
    std::fs::write(builtin_dir.join("README.txt"), "x").unwrap();
    let repo_dir = tempdir();

    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &builtin_dir);

    let err = engine
        .create_task(&project.id, "x", "t", "hi", json!({}))
        .await
        .unwrap_err();
    let CreateTaskError::Resolve(ResolveError::NotFound(message)) = &err else {
        panic!("expected Resolve(NotFound), got {err:?}");
    };
    let expected_repo_path = repo_dir
        .join(".chocofactory")
        .join("workflows")
        .join("x.yaml");
    assert!(
        message.contains(&expected_repo_path.to_string_lossy().to_string()),
        "{message}"
    );
    assert!(
        message.contains("built-in workflows: alpha, zeta"),
        "{message}"
    );
}

/// A project without `repo_path` resolves to the built-in only, and a
/// file of the same name in the legacy folder is ignored (#129).
#[tokio::test]
async fn create_task_without_a_project_repo_uses_the_builtin_and_ignores_the_legacy_folder() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    let legacy_dir = tempdir();
    write_chat_workflow(&builtin_dir);
    std::fs::write(
        legacy_dir.join("chat.yaml"),
        one_role_workflow_yaml("chat", "opus"),
    )
    .unwrap();
    std::fs::write(
        legacy_dir.join("legacy-only.yaml"),
        one_role_workflow_yaml("legacy-only", "opus"),
    )
    .unwrap();
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    )
    .with_legacy_workflows_dir(legacy_dir.0.clone());

    let task = engine
        .create_task(&project.id, "chat", "t", "hi", json!({}))
        .await
        .unwrap();
    assert_eq!(
        task.workflow_path.as_deref(),
        Some(format!("builtin:chat@{}", chocofactory_core::version::VERSION).as_str())
    );
    let chat_yaml = std::fs::read(builtin_dir.join("chat.yaml")).unwrap();
    assert_eq!(
        task.workflow_sha256.as_deref(),
        Some(sha256_hex(&chat_yaml).as_str())
    );

    let err = engine
        .create_task(&project.id, "legacy-only", "t", "hi", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CreateTaskError::Resolve(ResolveError::NotFound(_))
    ));
}

// ---- explicit workflow files, built-in follow semantics (#129) ----

#[tokio::test]
async fn create_task_from_a_file_records_the_canonical_path_and_wins_over_the_repo() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    let repo_dir = tempdir();
    let file_dir = tempdir();
    write_repo_workflow(&repo_dir, "mine", &one_role_workflow_yaml("mine", "opus"));
    let file_yaml = one_role_workflow_yaml("mine", "sonnet");
    let file_path = file_dir.join("whatever.yaml");
    std::fs::write(&file_path, &file_yaml).unwrap();
    std::fs::write(file_dir.join("prompt.md"), "hello").unwrap();
    let link = file_dir.join("link.yaml");
    std::os::unix::fs::symlink(&file_path, &link).unwrap();

    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    );

    // Through a symlink, so the canonical path is the target.
    let task = engine
        .create_task_from(
            &project.id,
            WorkflowRef::File(link.clone()),
            "t",
            "hi",
            json!({}),
        )
        .await
        .unwrap();
    let canonical = std::fs::canonicalize(&file_path).unwrap();
    assert_eq!(
        task.workflow_path.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(
        task.workflow_sha256.as_deref(),
        Some(sha256_hex(file_yaml.as_bytes()).as_str())
    );
    // `workflow_def` is the YAML's own `name:`, and the file beat the
    // repo workflow of the same name.
    assert_eq!(task.workflow_def, "mine");

    // Relative path: refused before touching the filesystem.
    let err = engine
        .create_task_from(
            &project.id,
            WorkflowRef::File(PathBuf::from("rel/x.yaml")),
            "t",
            "hi",
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::WorkflowFileNotAbsolute(_)));

    // Missing file: the canonicalize error, naming the path.
    let missing = file_dir.join("nope.yaml");
    let err = engine
        .create_task_from(
            &project.id,
            WorkflowRef::File(missing.clone()),
            "t",
            "hi",
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CreateTaskError::Canonicalize { path, .. } if *path == missing),
        "{err:?}"
    );
}

#[tokio::test]
async fn create_task_from_a_file_resolves_prompt_files_next_to_it() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    let file_dir = tempdir();
    std::fs::write(file_dir.join("prompt.md"), "hello").unwrap();
    let yaml = one_role_workflow_yaml("with-prompt", "sonnet").replace(
        "    role: chat
",
        "    role: chat
    prompt_file: prompt.md
",
    );
    let path = file_dir.join("wf.yaml");
    std::fs::write(&path, yaml).unwrap();
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    );

    engine
        .create_task_from(&project.id, WorkflowRef::File(path), "t", "hi", json!({}))
        .await
        .unwrap();
}

/// A built-in task's next stage loads the built-in directory's *current*
/// YAML, not the version it started on.
#[tokio::test]
async fn a_builtin_task_follows_the_current_builtin_yaml() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    std::fs::write(
        builtin_dir.join("x.yaml"),
        one_role_workflow_yaml("x", "sonnet"),
    )
    .unwrap();
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    );
    let task = engine
        .create_task(&project.id, "x", "t", "hi", json!({}))
        .await
        .unwrap();

    let before = engine.load_task_workflow(&task).await.unwrap();
    assert!(before.stages.contains_key("chatting"));

    std::fs::write(
        builtin_dir.join("x.yaml"),
        "name: x\nstages:\n  other_stage:\n    kind: terminal\n",
    )
    .unwrap();
    let after = engine.load_task_workflow(&task).await.unwrap();
    assert!(after.stages.contains_key("other_stage"));
    assert!(!after.stages.contains_key("chatting"));
}

/// A built-in that disappeared: the reload fails with `BuiltinGone`, and
/// the restart sweep parks the task stuck with a reason naming it.
#[tokio::test]
async fn a_builtin_that_disappeared_fails_the_reload_and_sticks_the_task() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    write_chat_workflow(&builtin_dir);
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    );
    let task = engine
        .create_task(&project.id, "chat", "t", "hi", json!({}))
        .await
        .unwrap();

    std::fs::remove_file(builtin_dir.join("chat.yaml")).unwrap();

    let err = engine.load_task_workflow(&task).await.unwrap_err();
    assert!(
        matches!(&err, LoadTaskWorkflowError::BuiltinGone(name) if name == "chat"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "the built-in workflow 'chat' is not part of this version of chocofactoryd"
    );
    let err = engine
        .send_message_or_resume(&task.id, "hello?")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SendMessageOrResumeError::BuiltinWorkflowGone(_)),
        "{err:?}"
    );

    engine.park_interrupted_turns().await.unwrap();
    let task = tasks::get(&pool, &task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TASK_STATUS_STUCK);
    let reason = task.stuck_reason.unwrap();
    assert!(reason.contains("built-in workflow 'chat'"), "{reason}");
}

/// A task that recorded a path into the old global folder keeps loading
/// that file; a pre-#88 task (NULL path) loads the legacy dir's file when
/// present and the built-in otherwise.
#[tokio::test]
async fn legacy_records_keep_loading() {
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    let legacy_dir = tempdir();
    std::fs::write(
        builtin_dir.join("x.yaml"),
        "name: x\nstages:\n  from_builtin:\n    kind: terminal\n",
    )
    .unwrap();
    std::fs::write(
        legacy_dir.join("x.yaml"),
        "name: x\nstages:\n  from_legacy:\n    kind: terminal\n",
    )
    .unwrap();
    std::fs::write(
        legacy_dir.join("y.yaml"),
        "name: y\nstages:\n  from_legacy:\n    kind: terminal\n",
    )
    .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &builtin_dir,
    )
    .with_legacy_workflows_dir(legacy_dir.0.clone());

    // Recorded path into the old folder.
    let recorded = seed_task(&pool, "y").await;
    let path = legacy_dir.join("y.yaml");
    sqlx::query("UPDATE tasks SET workflow_path = ? WHERE id = ?")
        .bind(path.to_string_lossy().as_ref())
        .bind(&recorded)
        .execute(&pool)
        .await
        .unwrap();
    let task = tasks::get(&pool, &recorded).await.unwrap().unwrap();
    let def = engine.load_task_workflow(&task).await.unwrap();
    assert!(def.stages.contains_key("from_legacy"));

    // NULL path: the legacy file wins while present...
    let null_task_id = seed_task(&pool, "x").await;
    let null_task = tasks::get(&pool, &null_task_id).await.unwrap().unwrap();
    assert!(null_task.workflow_path.is_none());
    let def = engine.load_task_workflow(&null_task).await.unwrap();
    assert!(def.stages.contains_key("from_legacy"));
    // ...and the built-in is used once it is gone.
    std::fs::remove_file(legacy_dir.join("x.yaml")).unwrap();
    let def = engine.load_task_workflow(&null_task).await.unwrap();
    assert!(def.stages.contains_key("from_builtin"));
}

#[test]
fn builtin_refs_round_trip() {
    let r = builtin_ref("coding-task");
    assert_eq!(
        r,
        format!(
            "builtin:coding-task@{}",
            chocofactory_core::version::VERSION
        )
    );
    assert_eq!(parse_builtin_ref(&r), Some("coding-task"));
    assert_eq!(parse_builtin_ref("builtin:chat@whatever"), Some("chat"));
    assert_eq!(parse_builtin_ref("builtin:chat"), None);
    assert_eq!(parse_builtin_ref("builtin:../x@1"), None);
    assert_eq!(parse_builtin_ref("builtin:@1"), None);
    assert_eq!(parse_builtin_ref("/abs/path/chat.yaml"), None);
}

/// A built-in's `script_file` is run as a real executable next to the YAML.
#[tokio::test]
async fn a_builtin_shell_stage_runs_its_script_file() {
    use std::os::unix::fs::PermissionsExt;
    let pool = connect_in_memory().await.unwrap();
    let builtin_dir = tempdir();
    std::fs::create_dir_all(builtin_dir.join("scripts")).unwrap();
    let script = builtin_dir.join("scripts").join("hello.sh");
    std::fs::write(&script, "#!/bin/sh\necho builtin-script-marker\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::write(
        builtin_dir.join("scripted.yaml"),
        r#"
name: scripted
stages:
  run:
    kind: shell
    script_file: scripts/hello.sh
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .unwrap();
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &builtin_dir);
    let task = engine
        .create_task(&project.id, "scripted", "t", "hi", json!({}))
        .await
        .unwrap();
    wait_until_task_status(&pool, &task.id, "closed").await;
    let state = workflow_state::get(&pool, &task.id).await.unwrap().unwrap();
    assert!(
        state.payload.to_string().contains("builtin-script-marker"),
        "{}",
        state.payload
    );
}

/// The materialized real `coding-task.yaml` loads, with `open_pr`'s
/// script an executable file under `.builtin-workflows/scripts/`.
#[test]
fn the_materialized_coding_task_gives_open_pr_an_executable_script() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir();
    let dir = root.join(".builtin-workflows");
    config_root::materialize_builtins(&dir).unwrap();
    let (def, _) = load_workflow_file(
        &dir.join("coding-task.yaml"),
        &Registry::single(Arc::new(ClaudeAdapter::new())),
    )
    .unwrap();
    let stage = def.stages.get("open_pr").expect("open_pr stage");
    let crate::workflow_def::StageKind::Shell { command, .. } = &stage.kind else {
        panic!("open_pr should be a shell stage: {:?}", stage.kind);
    };
    let crate::workflow_def::ShellCommand::ScriptFile(path) = command else {
        panic!("open_pr should use a script_file: {command:?}");
    };
    assert_eq!(path, &dir.join("scripts").join("open-pr.sh"));
    assert!(path.metadata().unwrap().permissions().mode() & 0o111 != 0);
}

/// `config.cwd` absent and `project.repo_path` set: `create_task` fills
/// it in, so the repo is visible in the task's config exactly as if
/// `--repo`/`config.cwd` had been passed directly.
#[tokio::test]
async fn create_task_fills_config_cwd_from_the_projects_repo_path_when_absent() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    write_chat_workflow(&global_dir);
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    let task = engine
        .create_task(&project.id, "chat", "t", "hi", json!({}))
        .await
        .unwrap();

    assert_eq!(task.config["cwd"], repo_dir.to_string_lossy().as_ref());
}

/// An explicit `config.cwd` (what `--repo` sends) wins over the
/// project's own `repo_path`.
#[tokio::test]
async fn create_task_explicit_cwd_wins_over_the_projects_repo_path() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    let explicit_dir = tempdir();
    write_chat_workflow(&global_dir);
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    let task = engine
        .create_task(
            &project.id,
            "chat",
            "t",
            "hi",
            json!({ "cwd": explicit_dir.to_string_lossy() }),
        )
        .await
        .unwrap();

    assert_eq!(task.config["cwd"], explicit_dir.to_string_lossy().as_ref());
}

/// A repo workflow's `prompt_file` resolves relative to the repo's own
/// `.chocofactory/workflows/` directory, not the global one — the
/// standard `WorkflowDefinition::load` behaviour, exercised through the
/// new repo-first resolution path.
#[tokio::test]
async fn create_task_resolves_prompt_files_relative_to_the_repo_workflow_file() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    let workflows_dir = repo_dir.join(".chocofactory").join("workflows");
    std::fs::create_dir_all(&workflows_dir).unwrap();
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

    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    engine
        .create_task(&project.id, "templated", "t", "hi", json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn send_message_reaches_the_live_session_started_by_create_task() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(&project_id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();

    engine
        .send_message(&task.id, "actually check the other branch too")
        .await
        .unwrap();

    // fake_claude.py echoes each line it receives as `echo:<text>` in
    // an assistant message event — proves the follow-up reached the
    // same live process this task's create_task call started.
    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    wait_until_events_contain(
        &pool,
        &runs[0].id,
        "echo:actually check the other branch too",
    )
    .await;
}

/// `send_message_or_resume` on a task created from a repo workflow
/// reloads the *recorded* `workflow_path` (issue #88), not a fresh
/// by-name lookup — proved by writing a different, broken `chat.yaml`
/// into the global directory *after* the task was created (with no
/// `stages.chatting`, so if the engine ever loaded it instead, the
/// stage lookup would fail with `UnknownStage`) and confirming the send
/// still succeeds against the repo file.
#[tokio::test]
async fn send_message_or_resume_reloads_the_recorded_repo_workflow_path() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    write_repo_workflow(&repo_dir, "chat", &one_role_workflow_yaml("chat", "sonnet"));
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    let task = engine
        .create_task(&project.id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();

    // A different `chat.yaml` lands in the global directory after
    // creation, with no `chatting` stage at all.
    std::fs::write(
        global_dir.join("chat.yaml"),
        "name: chat\nstages:\n  other_stage:\n    kind: terminal\n",
    )
    .unwrap();

    engine
        .send_message_or_resume(&task.id, "still there?")
        .await
        .unwrap();

    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    wait_until_events_contain(&pool, &runs[0].id, "echo:still there?").await;
}

/// Deleting the recorded workflow file makes the send fail with the
/// missing-file error — not a silent fallback to the global directory,
/// even though a `chat.yaml` sits right there.
#[tokio::test]
async fn send_message_or_resume_on_a_task_whose_recorded_file_was_deleted_fails_not_falls_back() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    write_chat_workflow(&global_dir);
    let repo_workflow_path =
        write_repo_workflow(&repo_dir, "chat", &one_role_workflow_yaml("chat", "sonnet"));
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &global_dir,
    );

    let task = engine
        .create_task(&project.id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();

    std::fs::remove_file(&repo_workflow_path).unwrap();

    let err = engine
        .send_message_or_resume(&task.id, "hello?")
        .await
        .unwrap_err();
    // `send_message_or_resume` loads the task's workflow itself, before
    // it even knows which stage kind it's dispatching to, so the
    // missing-file error surfaces at its own top level here — not
    // wrapped in the `SendMessage(...)` variant `send_message`'s own
    // (redundant) reload would produce.
    assert!(
        matches!(&err, SendMessageOrResumeError::MissingWorkflowFile(_)),
        "expected a missing-file error, not a silent fallback to the global chat.yaml: {err:?}"
    );
}

/// A legacy task — `workflow_path: NULL`, as every task created before
/// this column existed has — still reloads by name from the global
/// directory, exactly as it always did.
#[tokio::test]
async fn send_message_or_resume_on_a_legacy_task_falls_back_to_a_name_lookup() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let task_id = seed_task(&pool, "chat").await;
    let def = Arc::new(WorkflowDefinition::load(&workflows_dir.join("chat.yaml")).unwrap());
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    engine
        .start_task(&task_id, &def, Some("hello"))
        .await
        .unwrap();
    assert!(
        tasks::get(&pool, &task_id)
            .await
            .unwrap()
            .unwrap()
            .workflow_path
            .is_none(),
        "seed_task must produce a legacy task with no recorded workflow_path"
    );

    engine
        .send_message_or_resume(&task_id, "still there?")
        .await
        .unwrap();

    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
    wait_until_events_contain(&pool, &runs[0].id, "echo:still there?").await;
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(&project_id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();

    // Wait for the initial turn's reply before sending the follow-up,
    // so the two round trips can't land out of order.
    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    wait_until_events_contain(&pool, &runs[0].id, "echo:hello").await;

    engine.send_message(&task.id, "again").await.unwrap();
    wait_until_events_contain(&pool, &runs[0].id, "echo:again").await;

    // `wait_until_events_contain` above only guarantees the reply
    // itself has landed, not the `turn_completed` line that follows it
    // in the same drain loop (#70) — wait for the full expected count
    // too, so this doesn't race a read against that still-pending
    // append.
    let events =
        crate::test_support::wait_until(&format!("7 events on session {}", runs[0].id), || async {
            let events = crate::db::events::list_for_session(&pool, &runs[0].id)
                .await
                .unwrap();
            if events.len() >= 7 {
                Ok(events)
            } else {
                Err(recent_texts(&events))
            }
        })
        .await;
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
    // `fake_claude.py` emits a `result` line after every turn, exactly
    // like the real CLI (#70) — normalized to `turn_completed` (chat
    // is a `Standing` session, so it's recorded but nothing acts on
    // it) rather than discarded.
    assert_eq!(
        kinds_and_text,
        vec![
            ("human_message".to_string(), Some("hello")),
            ("session_meta".to_string(), None),
            ("assistant_message".to_string(), Some("echo:hello")),
            ("turn_completed".to_string(), None),
            ("human_message".to_string(), Some("again")),
            ("assistant_message".to_string(), Some("echo:again")),
            ("turn_completed".to_string(), None),
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(&project_id, "templated", "t", "ignored", json!({}))
        .await
        .unwrap();

    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    wait_until_events_contain(&pool, &runs[0].id, "echo:Do the templated thing.").await;

    let events = crate::db::events::list_for_session(&pool, &runs[0].id)
        .await
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.event_type == chocofactory_core::models::EventType::HumanMessage),
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(&project_id, "has-outcome", "t", "hello", json!({}))
        .await
        .unwrap();

    let err = engine.send_message(&task.id, "hi again").await.unwrap_err();
    assert!(matches!(
        err,
        SendMessageError::StageNotOpenEnded(stage) if stage == "chatting"
    ));
}

#[tokio::test]
async fn send_message_errors_when_the_open_stage_has_no_session_yet() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);

    // workflow_state seeded directly, skipping create_task/start_task
    // (and therefore skipping the session it would have created) —
    // simulates a task whose entry stage never actually got entered.
    let task_id = seed_task(&pool, "chat").await;
    workflow_state::create(&pool, &task_id, "chatting", "agent_turn", json!({}))
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
    workflow_state::create(&pool, &task_id, "ghost-stage", "agent_turn", json!({}))
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

    // Recorded task-scoped (no session — a human_gate never opens a
    // session), unlike the chat path's session-scoped HumanMessage.
    let recorded = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::HumanMessage)
        .expect("expected a HumanMessage event");
    assert_eq!(recorded.session_id, None);
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

// ---- #61: the stage after a resumed human_gate that can't start ------

/// The regression this task closes: `advance_from_stage`'s `HumanGate`
/// arm used to map its error straight through, leaving the task `open`
/// in the *next* stage with nothing running when that stage failed to
/// start. This must fail on `main` before the fix — an engine whose
/// adapter binary can't be spawned, so `coding` never starts once
/// `gate` is resumed.
#[tokio::test]
async fn resuming_a_human_gate_into_a_stage_that_cannot_start_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
    let yaml = r#"
name: gate-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  gate:
    kind: human_gate
    on: { resumed: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
    // `retry_task` re-resolves the workflow from `workflows_dir` by
    // name, so the file has to be on disk for the recovery half of
    // this test to find it.
    std::fs::write(dir.join("gate-then-turn.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;

    let broken_engine =
        engine_with_adapter_and_workflows_dir(pool.clone(), "/no/such/binary-3f6c9a", &dir);
    broken_engine
        .start_task(&task_id, &def, None)
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "gate").await;

    let err = broken_engine
        .send_message_or_resume(&task_id, "approved")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SendMessageOrResumeError::Advance(_)),
        "{err:?}"
    );

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "coding");
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("coding") && reason.contains("gate") && reason.contains("could not"),
        "{reason:?}"
    );

    let error_events: Vec<_> = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::Error)
        .collect();
    assert_eq!(error_events.len(), 1, "{error_events:?}");
    assert_eq!(error_events[0].payload["stuck"], json!(true));

    // Recovery: `retry_task` from an engine with a working binary
    // re-enters `coding` (the stage that failed to start) and it runs
    // to completion.
    let fixed_engine =
        engine_with_adapter_and_workflows_dir(pool.clone(), &reply_binary(&dir, "ok"), &dir);
    fixed_engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap();

    wait_until_task_status(&pool, &task_id, "closed").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.stuck_reason, None);
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "finished");
}

/// The other branch of the `HumanGate` catch-all: the failure happens
/// *inside* `advance_from_stage`'s own transition, before
/// `workflow_state::update` ever commits the next stage, so
/// `stage_to_blame` reads the row back unchanged and blames the gate
/// itself rather than whatever comes after it.
///
/// `connect_in_memory`'s pool is a single SQLite connection
/// (`db/pool.rs`), so a trigger on `workflow_state` reliably fails the
/// very `UPDATE` `advance_from_stage` issues to move the task out of
/// `gate` — no timing needed, unlike a real transient DB error.
#[tokio::test]
async fn resuming_a_human_gate_whose_own_transition_fails_marks_the_task_stuck_at_the_gate() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    let yaml = r#"
name: gate-db-failure
stages:
  gate:
    kind: human_gate
    on: { resumed: gate2 }
  gate2:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;
    std::fs::write(workflows_dir.join("gate-db-failure.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &workflows_dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;

    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        "/no/such/binary-unused-1a2b3c",
        &workflows_dir,
    );
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "gate").await;

    // Fails every subsequent UPDATE on this table — in particular the
    // one `advance_from_stage` is about to issue for `gate`'s "resumed"
    // transition.
    sqlx::query(
        "CREATE TRIGGER fail_ws_update BEFORE UPDATE ON workflow_state
             BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let err = engine
        .send_message_or_resume(&task_id, "approved")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SendMessageOrResumeError::Advance(EngineError::Db(_))),
        "{err:?}"
    );

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    // The failed UPDATE never committed, so the task is still sitting
    // in `gate`, not `gate2`.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "gate");

    let reason = task.stuck_reason.unwrap();
    assert!(reason.contains("gate"), "{reason:?}");
    assert!(
        reason.contains("resumed but the transition failed"),
        "{reason:?}"
    );
    assert!(reason.contains("injected"), "{reason:?}");
    // Guards against the two reason texts being swapped: the "could not
    // be entered after" wording belongs to the other branch, where a
    // *different* stage than the gate is blamed.
    assert!(!reason.contains("could not be entered"), "{reason:?}");

    let error_events: Vec<_> = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::Error)
        .collect();
    assert_eq!(error_events.len(), 1, "{error_events:?}");
    assert_eq!(error_events[0].payload["stuck"], json!(true));
}

/// Exercises the actual production predicate,
/// `EngineError::is_benign_resume_race` — not a copy of its list — so
/// this fails the moment the production match drifts from what this
/// task documents as benign.
#[test]
fn is_benign_resume_race_accepts_exactly_the_excluded_variants() {
    assert!(
        EngineError::UnknownOutcome {
            stage: "gate".to_string(),
            outcome: "resumed".to_string(),
        }
        .is_benign_resume_race()
    );
    assert!(EngineError::TerminalStageHasNoTransitions("gate".to_string()).is_benign_resume_race());
    assert!(
        EngineError::StageMovedOn {
            expected: "gate".to_string(),
            actual: "coding".to_string(),
        }
        .is_benign_resume_race()
    );
    assert!(EngineError::TaskCancelled("t1".to_string()).is_benign_resume_race());

    assert!(!EngineError::NoWorkflowState.is_benign_resume_race());
    assert!(!EngineError::UnknownStage("gate".to_string()).is_benign_resume_race());
    assert!(
        !EngineError::Template {
            stage: "coding".to_string(),
            reason: "bad".to_string(),
        }
        .is_benign_resume_race()
    );
}

/// Fires several `send_message_or_resume` calls at the same gate
/// concurrently, the way
/// `concurrent_advance_calls_on_the_same_task_do_not_lose_updates` does
/// for `advance`. Only one can win `advance_from_stage`'s per-task
/// lock and actually move the task out of `gate`; every loser must see
/// one of the benign races above rather than being marked `stuck`.
///
/// The destination stage is deliberately another `human_gate`
/// (`gate2`), not a stage that runs to completion and closes the task.
/// `db::tasks::update_status` sets the new status unconditionally and
/// clears `stuck_reason` (tasks.rs), so a task that reaches `closed`
/// would silently overwrite any stray `stuck` a loser wrote underneath
/// it — masking exactly the regression this test exists to catch. With
/// `gate2` the task must still be `open`, waiting on a person, once
/// every handle has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_resumes_of_the_same_gate_do_not_mark_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    let yaml = r#"
name: gate-then-gate-concurrent
stages:
  gate:
    kind: human_gate
    on: { resumed: gate2 }
  gate2:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;
    std::fs::write(workflows_dir.join("gate-then-gate-concurrent.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &workflows_dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;

    // No `agent_turn` stage is involved, so the adapter binary is never
    // invoked — this test is about how many callers win the race, not
    // about a stage that can or can't start.
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        "/no/such/binary-unused-90c1e4",
        &workflows_dir,
    );
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "gate").await;

    let mut handles = Vec::new();
    for _ in 0..6 {
        let engine = Arc::clone(&engine);
        let task_id = task_id.clone();
        handles.push(tokio::spawn(async move {
            engine.send_message_or_resume(&task_id, "approved").await
        }));
    }

    let (mut ok_count, mut err_count) = (0, 0);
    for handle in handles {
        match handle.await.unwrap() {
            Ok(()) => ok_count += 1,
            Err(SendMessageOrResumeError::Advance(err)) => {
                assert!(
                    err.is_benign_resume_race(),
                    "a losing concurrent resume must fail with a benign race, not {err:?}"
                );
                err_count += 1;
            }
            // Nothing else is expected here: the task is never
            // cancelled or stuck in this test, so `send_message_or_resume`'s
            // own status/stage checks have nothing to reject a loser
            // on before it ever reaches `advance_from_stage` — every
            // loser's error must be one of the `Advance` races above.
            // Deliberately not a catch-all: a stray `TaskStuck` here
            // (the exact bug this test exists to catch) must fail the
            // test, not get logged and folded into `err_count`.
            Err(other) => panic!(
                "a losing concurrent resume must fail as a benign Advance race, not {other:?}"
            ),
        }
    }
    assert_eq!(ok_count, 1, "exactly one resume should win the race");
    assert_eq!(err_count, 5);

    wait_until_stage(&pool, &task_id, "gate2").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        task.status, "open",
        "a benign loser must not mark the task stuck"
    );
    assert_eq!(task.stuck_reason, None);
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "gate2");
}

#[tokio::test]
async fn send_message_or_resume_routes_an_open_agent_turn_to_send_message() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &fixture_binary("fake_claude.py"),
        &workflows_dir,
    );

    let task = engine
        .create_task(&project_id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();

    engine
        .send_message_or_resume(&task.id, "actually check the other branch too")
        .await
        .unwrap();

    let runs = sessions::list_for_task(&pool, &task.id).await.unwrap();
    wait_until_events_contain(
        &pool,
        &runs[0].id,
        "echo:actually check the other branch too",
    )
    .await;
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
/// events up by `session_id` — a poll stage opens no session, so its
/// entries are task-scoped with no run id at all.
async fn wait_until_poll_attempt_recorded(pool: &SqlitePool, task_id: &str) {
    crate::test_support::wait_until(
        &format!("a poll attempt to be recorded on task {task_id}"),
        || async {
            let n = poll_events(pool, task_id).await.len();
            if n > 0 {
                Ok(())
            } else {
                Err(format!("{n} poll events"))
            }
        },
    )
    .await
}

/// Waits for the entry a poll records when it resolves — the one
/// carrying a `note`, as opposed to the bare progress entries.
async fn wait_until_decisive_poll_event(pool: &SqlitePool, task_id: &str) -> Value {
    crate::test_support::wait_until(
        &format!("a decisive poll event on task {task_id}"),
        || async {
            let all = poll_events(pool, task_id).await;
            let seen = format!("{} poll events, none with a note", all.len());
            all.into_iter()
                .find(|payload| payload.get("note").is_some())
                .ok_or(seen)
        },
    )
    .await
}

async fn seed_task_in(pool: &SqlitePool, workflow_def: &str, cwd: &Path) -> String {
    let project_id = projects::create(pool, "demo", None).await.unwrap().id;
    tasks::create(
        pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def,
            title: "T",
            config: json!({ "cwd": cwd.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
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
    // A spawn failure's "error" outcome has no 'on:' edge on this
    // stage (only `green`/`red` do), so it's marked stuck (X-4, #61).
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("watch")),
        "{:?}",
        task.stuck_reason
    );
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

/// `finish_detached`'s catch-all (#61) had no test before this task:
/// a poll resolves cleanly, but the `agent_turn` its outcome routes
/// into can't start (a binary that can't be spawned). Kept fast — a
/// short interval and a command that resolves on its first attempt —
/// since `interval: 1s` is the loader's floor.
#[tokio::test]
async fn a_poll_resolving_into_a_stage_that_cannot_start_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
    let yaml = format!(
        r#"
name: poll-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  watch:
    kind: poll
    command: "echo SUCCESS"
    interval: 1s
{GREEN_OR_RED}
    on: {{ green: coding, red: failed, error: failed, timeout: stalled }}
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: {{ done: finished }}
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
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "/no/such/binary-3f6c9a");

    engine.start_task(&task_id, &def, None).await.unwrap();

    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "coding");
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("coding") && reason.contains("watch"),
        "{reason:?}"
    );
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
        "chocofactoryd-engine-test-{}",
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
    crate::test_support::wait_until(
        &format!("a shell_output event for stage '{stage}' on task {task_id}"),
        || async {
            let all = events::list_for_task(pool, task_id).await.unwrap();
            match all.iter().find(|e| {
                e.event_type == EventType::ShellOutput
                    && e.payload.get("stage").and_then(Value::as_str) == Some(stage)
            }) {
                Some(event) => Ok(event.payload.clone()),
                None => Err(recent_texts(&all)),
            }
        },
    )
    .await
}

async fn wait_until_turn_outcome_event(pool: &SqlitePool, task_id: &str) -> Value {
    crate::test_support::wait_until(
        &format!("a turn_outcome event on task {task_id}"),
        || async {
            let all = events::list_for_task(pool, task_id).await.unwrap();
            match all.iter().find(|e| e.event_type == EventType::TurnOutcome) {
                Some(event) => Ok(event.payload.clone()),
                None => Err(recent_texts(&all)),
            }
        },
    )
    .await
}

/// Finishes a `review` (`capture: json`) stage from its recorded reply
/// alone, with no `report_outcome` call, by running `finish_turn`
/// directly against a run that already went idle.
///
/// Since #90 no live turn gets here without reporting: a single-shot
/// turn completes only once it calls `report_outcome`, and a `capture:
/// json` stage then routes on that report. Parsing the verdict out of the
/// reply is still `finish_turn`'s fallback — when the report can't be
/// read back, or for an adapter with no tool channel — so the tests that
/// pin how a reply is parsed drive that path directly rather than through
/// a session that would now sit waiting to be nudged.
///
/// `reply` takes the same directives as `fake_claude_reply.py`: a leading
/// `TOOL\n` narrates and makes a tool call first, and `BLOCKS\n` splits
/// the rest on `|` into separate text blocks.
async fn finish_review_turn_from_reply(
    pool: &SqlitePool,
    engine: &Arc<WorkflowEngine>,
    def: &Arc<WorkflowDefinition>,
    task_id: &str,
    reply: &str,
) {
    workflow_state::create(pool, task_id, "review", "agent_turn", json!({}))
        .await
        .unwrap();
    let run_id = sessions::create(
        pool,
        sessions::NewSession {
            task_id,
            stage: "review",
            role: "reviewer",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap()
    .id;

    let (uses_tool, reply) = match reply.strip_prefix("TOOL\n") {
        Some(rest) => (true, rest),
        None => (false, reply),
    };
    let blocks: Vec<&str> = match reply.strip_prefix("BLOCKS\n") {
        Some(rest) => rest.split('|').collect(),
        None => vec![reply],
    };
    let mut recorded = Vec::new();
    if uses_tool {
        recorded.push((
            EventType::AssistantMessage,
            json!({ "text": "I'll read the diff first." }),
        ));
        recorded.push((
            EventType::ToolCall,
            json!({ "tool_use_id": "toolu_1", "tool": "Read", "input": { "path": "a.rs" } }),
        ));
        recorded.push((
                EventType::ToolResult,
                json!({ "tool_use_id": "toolu_1", "tool": "Read", "output": "fn main() {}", "is_error": false }),
            ));
    }
    for block in blocks {
        recorded.push((EventType::AssistantMessage, json!({ "text": block })));
    }
    recorded.push((EventType::TurnCompleted, json!({ "is_error": false })));
    for (event_type, payload) in recorded {
        events::append(pool, &run_id, event_type, payload)
            .await
            .unwrap();
    }
    sessions::update_status(pool, &run_id, SessionStatus::Idle, None, None)
        .await
        .unwrap();

    engine
        .finish_turn(task_id, def, "review", Some(Capture::Json), &run_id)
        .await;
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
    // `current_stage` reads `coding` before the stage's session exists,
    // so wait for the session itself, not just the stage.
    //
    // The fixture echoes back whatever prompt it was handed, so the
    // rendered text showing up as the reply proves what was sent.
    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
    wait_until_events_contain(&pool, &run.id, "echo:fix pr 42").await;
}

/// #60's own motivating case: a coder prompt references a reviewer's
/// feedback, but the review's capture is a plain text with no `feedback`
/// field. Before #60 this killed the task with
/// `workflow_state.current_stage` permanently stuck at `coding`; now the
/// turn runs with the placeholder blanked, and a note on the timeline says
/// which one. (A stage that hasn't run yet is not noted at all; this is the
/// field-miss path, through an agent turn's prompt.)
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

    let state = workflow_state::create(
        &pool,
        &task_id,
        "coding",
        "agent_turn",
        json!({ "stages": { "internal_review": "looks fine" } }),
    )
    .await
    .unwrap();
    engine
        .enter_stage(&task_id, &def, "coding", None, None, &state.payload, None)
        .await
        .unwrap();

    // The turn ran at all — with the missing feedback blanked, not a
    // stuck task and a dead subprocess.
    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
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
    assert_eq!(note.session_id, None);
}

/// A prompt that references a stage that hasn't run yet renders empty and
/// records no `template_unresolved` event, on the agent-turn path too.
#[tokio::test]
async fn a_prompt_naming_a_stage_that_has_not_run_records_no_note() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    fs::write(
        dir.join("coder-turn.md"),
        "address: {{ stages.internal_review.feedback }}",
    )
    .unwrap();
    let yaml = r#"
name: templated-not-run
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
    let runs = sessions::list_for_task(&pool, &task_id).await.unwrap();
    let run = runs.iter().find(|r| r.stage == "coding").unwrap();
    wait_until_events_contain(&pool, &run.id, "echo:address: ").await;
    assert!(
        events::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .into_iter()
            .all(|e| e.event_type != EventType::TemplateUnresolved)
    );
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
    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
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
    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
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
        note.session_id, None,
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        r#"{"outcome": "approved", "comments": "ship-it"}"#,
    )
    .await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        "TOOL\n{\"outcome\": \"approved\", \"n\": 1}",
    )
    .await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        "```json\n{\"outcome\": \"approved\"}\n```",
    )
    .await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        "BLOCKS\n{\"outcome\": \"approved\",| \"n\": 1}",
    )
    .await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(&pool, &engine, &def, &task_id, "sorry, I could not do it").await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        r#"{"comments": "no verdict here"}"#,
    )
    .await;
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
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(&pool, &engine, &def, &task_id, "not json at all").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "review");
    // Marked stuck (X-4, #61) so a human can find it and retry it.
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("review")),
        "{:?}",
        task.stuck_reason
    );

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
    // no `capture:` neither stores it nor routes on it — `task` and the
    // engine-owned `arrival` (#112) are the only payload keys present
    // (P2-7a).
    assert_eq!(
        payload_of(&pool, &task_id).await,
        json!({
            "task": {"input": null, "title": "T"},
            "arrival": {"from": "coding", "outcome": "done"},
            "finished_stages": ["coding"],
        })
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

/// Issue #73's central case: a `report_outcome` call routes a capturing
/// stage even though the reply carries no parseable verdict of its own —
/// the whole reason to prefer a tool over reverse-engineering one from
/// prose.
#[tokio::test]
async fn a_report_outcome_call_routes_a_capturing_stage() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let binary = reply_binary(
        &dir,
        "REPORT approved\nsome prose the reply parser can't use",
    );
    let engine = engine_with_adapter(pool.clone(), &binary);

    engine
        .start_task(&task_id, &def, Some("review this"))
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    assert_eq!(event["applied"], true, "got {event}");
    assert_eq!(event["source"], "tool", "got {event}");
    assert_eq!(
        payload_of(&pool, &task_id).await["stages"]["review"]["outcome"],
        "approved"
    );
}

/// Every agent turn gets the tool (§ design doc, "not the reviewer's"),
/// including a stage with no `capture:` — a coder that reports `blocked`
/// must not vanish, but must also not gain the power to park a stage
/// that has never been able to.
#[tokio::test]
async fn a_report_on_a_non_capturing_stage_is_recorded_but_does_not_route() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
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
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let binary = reply_binary(&dir, "REPORT blocked\nran out of disk space");
    let engine = engine_with_adapter(pool.clone(), &binary);

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    // Routing is untouched: `blocked` isn't a `done` edge and never has
    // to be one — the stage advances on `done` exactly as it would with
    // no report at all.
    wait_until_stage(&pool, &task_id, "finished").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    assert_eq!(event["outcome"], "done", "got {event}");
    assert_eq!(event["applied"], true, "got {event}");
    // `source` is null, not `"tool"`: the report never drove this
    // outcome (it's the same hardcoded `done` the stage would have
    // taken with no report at all) — the fact a report was made is
    // `note`'s job, not `source`'s (review, #75 round 2).
    assert!(event["source"].is_null(), "got {event}");
    assert!(
        event["note"]
            .as_str()
            .is_some_and(|note| note.contains("does not route")),
        "got {event}"
    );
    // A no-capture stage stores nothing under `stages.*` even when a
    // report was made — the report informs the timeline, not the
    // payload a later stage's template could read.
    assert!(
        payload_of(&pool, &task_id)
            .await
            .get("stages")
            .is_none_or(|stages| stages.get("coding").is_none()),
    );
}

/// Critical review finding (#75): a `capture: text` stage's report must
/// not route the workflow either — only `capture: json` earns that.
/// Mirrors the no-`capture:` case above, but for a stage that *does*
/// capture (the reply's own text), to prove the routing gate checks the
/// capture *kind*, not just whether the stage captures at all. The
/// report's outcome, `approved`, isn't even a declared `on:` edge here —
/// if it were allowed to route, the stage would park instead of ever
/// reaching `finished`.
#[tokio::test]
async fn a_report_on_a_capture_text_stage_is_recorded_but_does_not_route() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: texty
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let binary = reply_binary(&dir, "REPORT approved\nlooks good to me");
    let engine = engine_with_adapter(pool.clone(), &binary);

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    assert_eq!(event["outcome"], "done", "got {event}");
    assert_eq!(event["applied"], true, "got {event}");
    // `source` is `"reply"`, not `"tool"`: a `capture: text` stage's
    // outcome always comes from parsing its own reply (it's always
    // `done`), never from the report — the report only earns a `note`
    // here, not credit for the outcome (review, #75 round 2).
    assert_eq!(event["source"], "reply", "got {event}");
    assert!(
        event["note"]
            .as_str()
            .is_some_and(|note| note.contains("does not route")),
        "got {event}"
    );
    // Unlike a no-`capture:` stage, `capture: text` still keeps the
    // reply's text — the report only opts the stage out of *routing*,
    // not out of its own declared capture.
    assert_eq!(
        payload_of(&pool, &task_id).await["stages"]["coding"],
        "looks good to me"
    );
}

/// The tool is the primary path, but the reply is still the fallback
/// when no report can be read — #73's own repro (prose before a JSON
/// verdict) must still route via the text fallback.
#[tokio::test]
async fn prose_then_json_still_routes_via_the_text_fallback() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    finish_review_turn_from_reply(
        &pool,
        &engine,
        &def,
        &task_id,
        "Looks correct to me, compiles and passes tests.\n\n\
             {\"outcome\": \"approved\", \"feedback\": \"\"}",
    )
    .await;
    wait_until_stage(&pool, &task_id, "finished").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    assert_eq!(event["source"], "reply", "got {event}");
    assert!(
        event["note"]
            .as_str()
            .is_some_and(|note| note.contains("recovered")),
        "got {event}"
    );
}

/// "The last call wins" — a model that calls the tool twice (correcting
/// itself, or retrying after the tool rejected an off-list value) should
/// have its final word taken as the verdict.
#[tokio::test]
async fn the_last_of_two_report_outcome_calls_wins() {
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
    let binary = reply_binary(&dir, "REPORT changes_requested,approved\n");
    let engine = engine_with_adapter(pool.clone(), &binary);

    engine
        .start_task(&task_id, &def, Some("review this"))
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    assert_eq!(
        payload_of(&pool, &task_id).await["stages"]["review"]["outcome"],
        "approved"
    );
}

/// The same `MAX_CAPTURE_BYTES` ceiling `derive_capture` applies to a
/// reply also applies to a `report_outcome` call: an oversized report
/// must not be stored, and the stage must fall back to `done` rather
/// than trust reading an outcome out of a value that was never captured.
#[tokio::test]
async fn an_oversized_report_is_dropped_and_the_stage_falls_back_to_done() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(capturing_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    // `capturing_turn_yaml`'s `review` stage has a `done` edge, so an
    // oversized report's fallback to `TURN_DEFAULT_OUTCOME` still routes
    // — the point being proven is that the oversized value itself never
    // reaches the payload, not that the stage parks.
    let huge_outcome = "x".repeat(MAX_CAPTURE_BYTES + 1);
    let binary = reply_binary(&dir, &format!("REPORT {huge_outcome}\n"));
    let engine = engine_with_adapter(pool.clone(), &binary);

    engine
        .start_task(&task_id, &def, Some("review this"))
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let event = wait_until_turn_outcome_event(&pool, &task_id).await;
    assert_eq!(event["outcome"], "done", "got {event}");
    assert_eq!(event["applied"], true, "got {event}");
    assert_eq!(event["source"], "tool", "got {event}");
    assert!(
        event["note"]
            .as_str()
            .is_some_and(|note| note.contains("exceeds")),
        "got {event}"
    );
    assert!(
        payload_of(&pool, &task_id)
            .await
            .get("stages")
            .is_none_or(|stages| stages.get("review").is_none()),
        "an oversized report must not be captured into the payload"
    );
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

/// Symmetric with `outcome_from_report`'s trim (review, #75 round 2):
/// the report path and the reply path should treat a whitespace-padded
/// `outcome` the same way, not park one and route the other.
#[test]
fn turn_outcome_trims_whitespace_before_matching() {
    let captured = json!({"outcome": " approved \n"});
    let (outcome, note) = turn_outcome(Capture::Json, Some(&captured));
    assert_eq!(outcome, "approved");
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

#[test]
fn outcome_from_report_reads_the_reports_outcome_key() {
    let report = json!({"outcome": "approved", "summary": "looks right"});
    let (outcome, note) = outcome_from_report(&report);
    assert_eq!(outcome, "approved");
    assert!(note.is_none());
}

#[test]
fn outcome_from_report_falls_back_for_a_non_string_or_missing_outcome() {
    for report in [
        json!({"outcome": 7}),
        json!({"outcome": ""}),
        json!({"summary": "x"}),
    ] {
        let (outcome, note) = outcome_from_report(&report);
        assert_eq!(outcome, "done", "for {report}");
        assert!(note.is_some(), "for {report}");
    }
}

/// Review, #75: `choco mcp-serve` trims `outcome` before confirming
/// success to the model (`call_tool`), but the `ToolCall` event this
/// reads back records the model's argument verbatim. Without a matching
/// trim here, a whitespace-padded outcome the tool accepted would come
/// back off the timeline as a string `advance_from_stage` can't match
/// against the `on:` edge it names — parking a stage the tool just told
/// the model was routable.
#[test]
fn outcome_from_report_trims_whitespace_before_matching() {
    let report = json!({"outcome": " approved \n", "summary": ""});
    let (outcome, note) = outcome_from_report(&report);
    assert_eq!(outcome, "approved");
    assert!(note.is_none());
}

/// Whitespace-only, though non-empty before trimming, is the same as no
/// verdict at all — not a value to hand `advance_from_stage`.
#[test]
fn outcome_from_report_treats_whitespace_only_as_empty() {
    let report = json!({"outcome": "   ", "summary": ""});
    let (outcome, note) = outcome_from_report(&report);
    assert_eq!(outcome, "done");
    assert!(note.is_some());
}

/// #73's own repro, verbatim: the reviewer's exact reply from the
/// dogfood run against #50 that first surfaced this bug — prose, a blank
/// line, then a well-formed verdict.
#[test]
fn sole_top_level_json_object_recovers_hash_73s_original_repro() {
    let reply = "No PR opened yet, that's fine — my scope is reviewing the diff. The\n\
                      implementation is correct, faithful to the issue's proposed fix, compiles\n\
                      cleanly, passes fmt/clippy/tests, and is applied symmetrically to both files\n\
                      as requested.\n\n\
                      {\"outcome\": \"approved\", \"feedback\": \"\"}";
    let found = sole_top_level_json_object(reply).unwrap();
    let parsed: Value = serde_json::from_str(found).unwrap();
    assert_eq!(parsed["outcome"], "approved");
}

#[test]
fn sole_top_level_json_object_finds_an_object_after_prose_too() {
    assert_eq!(
        sole_top_level_json_object("{\"outcome\": \"approved\"} — done reviewing"),
        Some("{\"outcome\": \"approved\"}")
    );
}

/// `{a}` is brace-balanced but not valid JSON (`a` is a bareword) — it
/// must be recognised as prose that merely looks like an object, not a
/// second candidate that makes the real verdict ambiguous.
#[test]
fn sole_top_level_json_object_ignores_a_brace_balanced_non_json_span() {
    let reply = "prose {a} more prose {\"outcome\": \"approved\", \"note\": \"uses {braces}\"}";
    let found = sole_top_level_json_object(reply).unwrap();
    let parsed: Value = serde_json::from_str(found).unwrap();
    assert_eq!(parsed["outcome"], "approved");
    assert_eq!(parsed["note"], "uses {braces}");
}

#[test]
fn sole_top_level_json_object_handles_escaped_quotes_and_backslashes() {
    // A JSON string containing an escaped quote and an escaped
    // backslash — both must not be mistaken for the string's own end.
    let reply = r#"see {"outcome": "approved", "note": "she said \"ok\" then \\"}"#;
    let found = sole_top_level_json_object(reply).unwrap();
    let parsed: Value = serde_json::from_str(found).unwrap();
    assert_eq!(parsed["outcome"], "approved");
}

/// Review, #75 round 2 (finding #7): two *equally valid* JSON objects —
/// unlike the brace-balanced-but-invalid case above — must not be
/// resolved by picking the last one. An illustrative example verdict
/// followed by the real one is exactly this shape, and nothing about an
/// object's contents (from this function's point of view) says which one
/// is real.
#[test]
fn sole_top_level_json_object_is_none_when_two_valid_objects_compete() {
    assert_eq!(
        sole_top_level_json_object("first {\"a\": 1} then {\"outcome\": \"approved\"}"),
        None
    );
}

#[test]
fn sole_top_level_json_object_returns_none_for_no_object_or_an_unbalanced_one() {
    for reply in ["plain text", "", "unbalanced { still open", "closed } only"] {
        assert_eq!(sole_top_level_json_object(reply), None, "for {reply:?}");
    }
}

#[test]
fn sole_top_level_json_object_ignores_nested_objects_and_returns_the_whole_outer_one() {
    let reply = "{\"outcome\": \"approved\", \"nested\": {\"a\": 1}}";
    assert_eq!(sole_top_level_json_object(reply), Some(reply));
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
    assert_eq!(
        unresolved,
        vec![crate::template::Unresolved {
            placeholder: "{{ stages.open_pr.missing }}".to_string(),
            kind: crate::template::UnresolvedKind::Missing,
        }]
    );
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
        Registry::single(adapter),
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
) -> chocofactory_core::models::Session {
    crate::test_support::wait_until(
        &format!("a session for stage '{stage}' on task {task_id}"),
        || async {
            let all = sessions::list_for_task(pool, task_id).await.unwrap();
            let seen = format!(
                "sessions for stages {:?}",
                all.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>()
            );
            all.into_iter().find(|r| r.stage == stage).ok_or(seen)
        },
    )
    .await
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

    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    // Each role's global `cli` is a distinct adapter (every name a `cli:`
    // can take is a registry key), so the rows and the recorded calls show
    // which adapter each role actually ran on.
    let coder_adapter = RecordingAdapter::new(
        "coder-global-cli",
        &fixture_binary("fake_claude_echo_args.py"),
    );
    let reviewer_adapter = RecordingAdapter::new(
        "reviewer-global-cli",
        &fixture_binary("fake_claude_echo_args.py"),
    );
    let engine = engine_with_registry(
        pool.clone(),
        Registry::new(vec![coder_adapter.clone(), reviewer_adapter.clone()]),
        &workflows_dir,
        Some(&global_config_path),
    );

    // Overrides for *both* roles at once — the task-level layer #17 is
    // about being able to supply for more than one role.
    let task = engine
        .create_task(
            &project_id,
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
    assert_eq!(coder_adapter.calls(), vec![RecordedCall::Start]);
    assert_eq!(reviewer_adapter.calls(), vec![RecordedCall::Start]);

    // `model`: coder's task-level override beat the workflow-def's
    // `coder-def-model`; reviewer, unmentioned at the task level and
    // silent in the workflow def, fell through to global.
    assert_eq!(coder_run.model, "coder-task-model");
    assert_eq!(reviewer_run.model, "reviewer-global-model");

    // System prompts, read back off each subprocess's own argv: coder from
    // the workflow-def file, reviewer from its task-level inline text.
    // `multi-role.yaml` doesn't opt into `worktree: true`, so neither
    // role's spawn is sandboxed (#67) — `permission_mode` stays unset.
    wait_until_events_contain_prefix(
        &pool,
        &coder_run.id,
        "model=coder-task-model|system_prompt=you write code|permission_mode=<unset>|",
    )
    .await;
    wait_until_events_contain_prefix(
        &pool,
        &reviewer_run.id,
        "model=reviewer-global-model|system_prompt=inline reviewer prompt|permission_mode=<unset>|",
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

    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_global_config(
        pool.clone(),
        &fixture_binary("fake_claude_echo_args.py"),
        &workflows_dir,
        &global_config_path,
    );

    let task = engine
        .create_task(&project_id, "gated-review", "T", "go", json!({}))
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
    crate::test_support::wait_until(&format!("{path:?} to be removed"), || async {
        if !path.exists() {
            Ok(())
        } else {
            Err(format!("{path:?} still exists"))
        }
    })
    .await
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

// ---- branch cleanup on done and cancel (#102) ----

/// A repo with a bare `origin` (so a stage can `git push`).
async fn repo_with_origin() -> (TempDir, PathBuf) {
    let root = tempdir();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo).await;
    let bare = root.join("origin.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]).await;
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]).await;
    (root, repo)
}

async fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn branch_exists_in(repo: &Path, task_id: &str) -> bool {
    !git_stdout(repo, &["branch", "--list", &format!("task/{task_id}")])
        .await
        .is_empty()
}

async fn branch_events(pool: &SqlitePool, task_id: &str) -> Vec<Value> {
    events::list_for_task(pool, task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::BranchCleanup)
        .map(|e| e.payload)
        .collect()
}

/// Runs `shell` in the worktree of a one-shell-stage task and returns
/// once the task is closed.
async fn run_worktree_task_to_done(repo: &Path, shell: &str) -> (SqlitePool, String) {
    let pool = connect_in_memory().await.unwrap();
    let yaml = format!(
        r#"
name: branch-done-flow
worktree: true
stages:
  run:
    kind: shell
    command: {shell:?}
    on: {{ done: finished, error: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#
    );
    let def = Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap());
    let task_id = seed_task_in(&pool, &def.name, repo).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;
    let path = worktree::worktree_path(repo, "demo", &task_id).unwrap();
    wait_until_path_gone(&path).await;
    (pool, task_id)
}

const COMMIT: &str =
    "echo x > f && git add f && git -c user.email=a@b.c -c user.name=n commit -qm work";

#[tokio::test]
async fn done_deletes_a_pushed_branch_and_records_its_tip_first() {
    let (_root, repo) = repo_with_origin().await;
    let (pool, task_id) =
        run_worktree_task_to_done(&repo, &format!("{COMMIT} && git push -q -u origin HEAD")).await;
    let tip = git_stdout(&repo, &["rev-parse", &format!("origin/task/{task_id}")]).await;
    assert!(!tip.is_empty());
    crate::test_support::wait_until("the branch to be deleted", || async {
        if branch_exists_in(&repo, &task_id).await {
            Err("still there".to_string())
        } else {
            Ok(())
        }
    })
    .await;
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "deleting");
    assert_eq!(notes[0]["branch"], format!("task/{task_id}"));
    assert_eq!(notes[0]["sha"], tip.as_str());
    assert_eq!(
        notes[0]["message"],
        format!("deleting branch task/{task_id} at {tip}").as_str()
    );
}

#[tokio::test]
async fn done_keeps_an_unpushed_branch_and_says_why() {
    let (_root, repo) = repo_with_origin().await;
    let (pool, task_id) = run_worktree_task_to_done(&repo, COMMIT).await;
    crate::test_support::wait_until("a kept note", || async {
        if branch_events(&pool, &task_id).await.is_empty() {
            Err("no note yet".to_string())
        } else {
            Ok(())
        }
    })
    .await;
    assert!(branch_exists_in(&repo, &task_id).await);
    let tip = git_stdout(&repo, &["rev-parse", &format!("task/{task_id}")]).await;
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "kept");
    assert_eq!(notes[0]["sha"], tip.as_str());
    assert!(
        notes[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("not on any remote"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn done_leaves_the_branch_and_says_so_when_the_worktree_removal_fails() {
    let (_root, repo) = repo_with_origin().await;
    let pool = connect_in_memory().await.unwrap();
    // The stage locks its own worktree, so `worktree remove --force`
    // at the terminal stage fails.
    let yaml = format!(
        r#"
name: branch-done-locked
worktree: true
stages:
  run:
    kind: shell
    command: {cmd:?}
    on: {{ done: finished, error: failed }}
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: {{ resumed: finished }}
"#,
        cmd = format!("{COMMIT} && git worktree lock .")
    );
    let def = Arc::new(WorkflowDefinition::parse(&yaml, Path::new(".")).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &repo).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;
    crate::test_support::wait_until("a kept note", || async {
        if branch_events(&pool, &task_id).await.is_empty() {
            Err("no note yet".to_string())
        } else {
            Ok(())
        }
    })
    .await;
    let path = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
    assert!(path.exists());
    assert!(branch_exists_in(&repo, &task_id).await);
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "kept");
    assert_eq!(notes[0]["reason"], "worktree removal failed");
}

/// A worktree task parked at a human gate, with a commit
/// on its branch (pushed iff `push`). Returns what the tests inspect.
async fn cancellable_worktree_task(
    push: bool,
) -> (
    TempDir,
    Arc<WorkflowEngine>,
    SqlitePool,
    PathBuf,
    PathBuf,
    String,
    String,
) {
    let pool = connect_in_memory().await.unwrap();
    let (root, repo) = repo_with_origin().await;
    let yaml = r#"
name: wt-cancellable
worktree: true
stages:
  gate:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &repo).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "gate").await;
    let path = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
    fs::write(path.join("work.txt"), "work\n").unwrap();
    git(&path, &["add", "."]).await;
    git(
        &path,
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=n",
            "commit",
            "-qm",
            "work",
        ],
    )
    .await;
    if push {
        git(&path, &["push", "-q", "-u", "origin", "HEAD"]).await;
    }
    let tip = git_stdout(&path, &["rev-parse", "HEAD"]).await;
    (root, engine, pool, repo, path, task_id, tip)
}

#[tokio::test]
async fn cancel_deletes_worktree_and_unpushed_branch_and_records_the_tip() {
    let (_root, engine, pool, repo, path, task_id, tip) = cancellable_worktree_task(false).await;
    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(!path.exists());
    assert!(!branch_exists_in(&repo, &task_id).await);
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "deleting");
    assert_eq!(notes[0]["sha"], tip.as_str());
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
    assert!(!task.kept_work);
}

#[tokio::test]
async fn cancel_deletes_a_pushed_branch_too() {
    let (_root, engine, _pool, repo, path, task_id, _tip) = cancellable_worktree_task(true).await;
    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(!path.exists());
    assert!(!branch_exists_in(&repo, &task_id).await);
}

/// If the `deleting` note can't be written, `git branch -D` must not
/// run: the recorded SHA is the only way back to the work.
#[tokio::test]
async fn cancel_keeps_the_branch_when_the_tip_cannot_be_recorded() {
    let (_root, engine, pool, repo, path, task_id, tip) = cancellable_worktree_task(false).await;
    sqlx::query(
        "CREATE TRIGGER fail_deleting BEFORE INSERT ON events \
             WHEN NEW.event_type='branch_cleanup' \
             AND json_extract(NEW.payload,'$.action')='deleting' \
             BEGIN SELECT RAISE(ABORT,'injected'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();
    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(!path.exists());
    assert!(branch_exists_in(&repo, &task_id).await);
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "kept");
    assert_eq!(notes[0]["sha"], tip.as_str());
    assert_eq!(
        notes[0]["reason"],
        "could not record the branch tip on the timeline"
    );
}

/// A failing `git branch -D` is logged and put on the timeline after
/// the `deleting` note; the cancel itself still succeeds.
#[tokio::test]
async fn cancel_records_a_failed_branch_delete_on_the_timeline() {
    let (_root, engine, pool, repo, path, task_id, tip) = cancellable_worktree_task(false).await;
    fs::write(
        repo.join(format!(".git/refs/heads/task/{task_id}.lock")),
        "",
    )
    .unwrap();
    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(!path.exists());
    assert!(branch_exists_in(&repo, &task_id).await);
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(notes[0]["action"], "deleting");
    assert_eq!(notes[0]["sha"], tip.as_str());
    assert_eq!(notes[1]["action"], "delete_failed");
    assert!(!notes[1]["error"].as_str().unwrap().is_empty());
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
}

/// When the worktree can't be removed the branch is left alone, and the
/// timeline says why.
#[tokio::test]
async fn cancel_leaves_the_branch_when_the_worktree_removal_fails() {
    let (_root, engine, pool, repo, path, task_id, _tip) = cancellable_worktree_task(false).await;
    git(&repo, &["worktree", "lock", path.to_str().unwrap()]).await;
    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(path.exists());
    assert!(branch_exists_in(&repo, &task_id).await);
    let notes = branch_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["action"], "kept");
    assert_eq!(notes[0]["reason"], "worktree removal failed");
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
}

#[tokio::test]
async fn cancel_with_keep_leaves_worktree_and_branch_and_sets_the_flag() {
    let (_root, engine, pool, repo, path, task_id, _tip) = cancellable_worktree_task(false).await;
    engine.cancel_task(&task_id, true).await.unwrap();
    assert!(path.exists(), "the worktree must be kept");
    assert!(
        branch_exists_in(&repo, &task_id).await,
        "the branch must be kept"
    );
    assert!(branch_events(&pool, &task_id).await.is_empty());
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
    assert!(task.kept_work);
}

/// The flag and the status are one statement: `mark_cancelled` is the
/// only writer, and it sets both. Pinned at the db layer, where a
/// second write would have to appear.
#[tokio::test]
async fn mark_cancelled_sets_status_and_flag_in_one_write() {
    let pool = connect_in_memory().await.unwrap();
    let task_id = seed_task(&pool, "wf").await;
    let task = tasks::mark_cancelled(&pool, &task_id, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, "cancelled");
    assert!(
        task.kept_work,
        "the row returned by the one UPDATE has both"
    );
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
    projects::update(&pool, &project_id, Some("renamed"), None)
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
/// `reviewer` — both roles run on the one claude adapter, which has one
/// fixed binary, so distinguishing the two roles *within* that adapter has
/// to happen inside the script itself. (Which adapter a role runs on is
/// picked by its `cli:`; see the dispatch tests for that.) `adapter/claude.rs::spawn` passes
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
        *"planning agent"*) role="planner" ;;
    esac
done
export MOCK_CLAUDE_ONESHOT=1
if [ "$role" = "planner" ]; then
    # Call N reads `planner-reply-N.json` when present, else
    # `planner-reply.json`, so a test can script a different report per call.
    n=$(cat "{dir}/planner-calls" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "{dir}/planner-calls"
    f="{dir}/planner-reply-$n.json"
    [ -f "$f" ] || f="{dir}/planner-reply.json"
    export MOCK_CLAUDE_REPLY="planned"
    export MOCK_CLAUDE_REPORT="$(cat "$f")"
elif [ "$role" = "reviewer" ]; then
    export MOCK_CLAUDE_REPLY="$(cat "{reply_path}")"
    export MOCK_CLAUDE_REPORT="$(cat "{reply_path}")"
else
    export MOCK_CLAUDE_REPLY="did the thing"
fi
exec "{mock_claude}" "$@"
"#,
            reply_path = reply_path.display(),
            mock_claude = mock_claude.display(),
            dir = dir.display(),
        ),
    )
}

/// Serialises every test that installs a `gh` stub on `PATH`.
///
/// There is more than one such test now (#78 added the
/// changes-requested lap alongside the happy path), and each points
/// `PATH` at its *own* stub directory. Without this they interleave:
/// the second test's `set_var` replaces the first's, so the first
/// test's `gh` resolves to the second's stub — reading the wrong
/// `verdict` file and appending to the wrong `pr-created` log. That
/// shows up as the two failures this lock exists to prevent: a happy
/// path that never reaches `done`, and a create counted twice.
static PATH_GUARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Prepends `dir` to `PATH` for the process, restoring the original
/// value on drop.
///
/// Mutating a whole test process's environment for one test is a race
/// against every other test running concurrently in the same process.
/// `PATH_GUARD_LOCK` above makes the `gh` tests take turns with each
/// other, and grepping this crate confirms no *other* test shells out
/// to a bare `gh` (every other `"gh ..."` string in the suite is
/// loader/template text that's only ever parsed or rendered, never
/// executed) — so in practice no test observes a `PATH` it didn't
/// install.
///
/// Be clear about what that does and doesn't buy. Rust 2024's
/// `set_var` contract is "no other thread concurrently accesses the
/// environment", and libc's `getenv` — reached from tokio, sqlx, the
/// TLS stack — does not take this lock. The lock removes the
/// cross-test interference that actually bites; it does not discharge
/// the `unsafe`. Threading a per-command env override through
/// `shell::run` would, and is the real fix if this grows a third
/// caller.
struct PathPrefixGuard {
    original: Option<std::ffi::OsString>,
    /// Held for the guard's whole life, not just `new`: the exclusion
    /// has to cover the test *body*, which is when the stub actually
    /// runs, not merely the moment `PATH` is written.
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PathPrefixGuard {
    fn new(dir: &Path) -> Self {
        // Poisoning is irrelevant: the guarded data is `()`, and a
        // test that panicked while holding this must not stop every
        // later one from running.
        let _lock = PATH_GUARD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Read under the lock, never before it: reading first could
        // capture a `PATH` another such test had already prefixed and
        // then "restore" that on drop, leaking a dead directory into
        // every later test's `PATH`.
        let original = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir);
        if let Some(existing) = &original {
            new_path.push(":");
            new_path.push(existing);
        }
        // SAFETY: see struct doc comment.
        unsafe { std::env::set_var("PATH", new_path) };
        PathPrefixGuard { original, _lock }
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

/// A page of PR comments holding one owner comment, newer than the
/// stub's head commit date. `body` is spliced into JSON as is, so
/// newlines are written `\\n`.
fn owner_comment_page(body: &str) -> String {
    format!(
        r#"[{{"created_at": "2030-01-01T00:00:00Z", "updated_at": "2030-01-01T00:00:00Z", "author_association": "OWNER", "user": {{"login": "owner"}}, "html_url": "https://example.test/c/1", "body": "{body}"}}]"#
    )
}

/// A stub `gh` covering exactly the invocations `coding-task.yaml`
/// makes: `pr create`, `pr checks`, `pr list` (`open_pr`'s existence
/// probe and its number/url read-back), `pr view` (the head SHA), and
/// `gh api` (the head commit's date, then the comment list). Backed by
/// real `git`/a real local bare repo for everything else. Returns the
/// directory to prepend to `PATH`.
///
/// Stateful on purpose, because that is the behaviour #78's fixes turn
/// on: `pr create` records that it ran so the probe can find nothing
/// before it and something after, and the review calls read files the
/// test owns: `verdict` (issue comments), `reviews` and `review-comments`.
///
/// What it deliberately does *not* model: PR state (so the
/// `--state open` scoping has no regression test here), and the jq
/// selection rules (the stub applies each call's `-q` filter with `jq`
/// to the canned `verdict`, `reviews` and `review-comments` files, and
/// does not model pagination). `tests/await_review_script.rs` covers the selection
/// directly against the shipped script.
fn gh_stub_dir(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "gh",
        &format!(
            r#"#!/bin/sh
set -eu
created="{dir}/pr-created"
case "$1" in
    api)
        # `awaiting_human_review` runs `scripts/await-review.sh`, which
        # makes the head commit's date call, then the PR's comments, its
        # reviews and its inline review comments. The stub routes by the
        # endpoint path and answers like gh would: it applies the call's
        # `-q` filter (with `jq`) to a canned page the test owns: the
        # `verdict` file (a JSON array of issue comments), the `reviews`
        # file and the `review-comments` file (empty lists when absent).
        # What an item has to look like to vote is covered in
        # `tests/await_review_script.rs`; these workflow tests cover the
        # routing either side of it.
        url=""
        for a in "$@"; do
            case "$a" in repos/*) url=$a ;; esac
        done
        q=""; prev=""
        for a in "$@"; do
            if [ "$prev" = "-q" ]; then q=$a; fi
            prev=$a
        done
        case "$url" in
            */issues/*/comments*)
                # A test can hold the comments call: while `hold` exists the stub
                # touches `held` and waits for `release` before answering. A
                # no-op unless `hold` exists.
                if [ -e "{dir}/hold" ]; then
                    touch "{dir}/held"
                    while [ ! -e "{dir}/release" ]; do sleep 0.05; done
                fi
                jq -r "$q" < "{dir}/verdict"
                ;;
            */pulls/*/reviews*)
                if [ -e "{dir}/reviews" ]; then jq -r "$q" < "{dir}/reviews"; else echo '[]' | jq -r "$q"; fi
                ;;
            */pulls/*/comments*)
                if [ -e "{dir}/review-comments" ]; then jq -r "$q" < "{dir}/review-comments"; else echo '[]' | jq -r "$q"; fi
                ;;
            *)
                echo "2020-01-01T00:00:00Z"
                ;;
        esac
        ;;
    pr)
        case "$2" in
            create)
                echo created >> "$created"
                echo "https://example.test/pr/42"
                ;;
            list)
                # `open_pr`'s probe and its read-back, both scoped to open
                # PRs. Empty until `pr create` has run, so the first lap
                # creates and every later lap reuses.
                if printf '%s\n' "$@" | grep -q headRefOid && ! printf '%s\n' "$@" | grep -q url; then
                    # `open_pr`'s pre-push head lookup: no head commit, so
                    # the push stays a plain one (a later lap only adds commits).
                    :
                elif [ -s "$created" ]; then
                    if printf '%s\n' "$@" | grep -q url; then
                        echo '{{"number": 42, "url": "https://example.test/pr/42"}}'
                    else
                        echo 42
                    fi
                fi
                ;;
            view)
                # One read of head, state and merge time (#102), answered
                # like gh would: the call's `-q` filter applied with `jq`.
                # The PR is OPEN unless the test has created `pr-merged`.
                q=""; prev=""
                for a in "$@"; do
                    if [ "$prev" = "-q" ]; then q=$a; fi
                    prev=$a
                done
                state=OPEN; merged=null
                if [ -e "{dir}/pr-merged" ]; then
                    state=MERGED; merged='"2030-01-02T00:00:00Z"'
                fi
                if [ -z "$q" ]; then
                    # `open_pr`'s body read-back (`-t`), not the verdict poll.
                    echo "0000000000000000000000000000000000000000"
                else
                    printf '{{"headRefOid":"0000000000000000000000000000000000000000","state":"%s","mergedAt":%s,"statusCheckRollup":[{{"state":"SUCCESS"}}]}}' "$state" "$merged" | jq -r "$q"
                fi
                ;;
            checks)
                # `checks_polling`: one SUCCESS check, `-q` applied like gh does.
                q=""; prev=""
                for a in "$@"; do
                    if [ "$prev" = "-q" ]; then q=$a; fi
                    prev=$a
                done
                echo '[{{"name":"build","state":"SUCCESS"}}]' | jq -r "$q"
                ;;
            *)
                echo "stub gh: unhandled pr subcommand: $*" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "stub gh: unhandled subcommand: $*" >&2
        exit 1
        ;;
esac
"#,
            dir = dir.to_string_lossy(),
        ),
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
    seed_builtin_coding_task(pool, repo, scripts_dir, reviewer_reply, "coding-task").await
}

/// `seed_coding_task` for any shipped coding workflow, named by its file
/// stem (`coding-task`, `coding-task-planned`).
async fn seed_builtin_coding_task(
    pool: &SqlitePool,
    repo: &Path,
    scripts_dir: &Path,
    reviewer_reply: &str,
    workflow: &str,
) -> (String, Arc<WorkflowDefinition>, PathBuf) {
    let def = Arc::new(
        WorkflowDefinition::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../workflows/{workflow}.yaml"))
                .as_path(),
        )
        .unwrap_or_else(|e| panic!("workflows/{workflow}.yaml failed to load: {e}")),
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

    let project_id = projects::create(pool, "demo", None).await.unwrap().id;
    let task_id = tasks::create(
        pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: &def.name,
            title: "Add a small feature",
            config: json!({ "cwd": repo.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
        },
    )
    .await
    .unwrap()
    .id;

    (task_id, def, claude_wrapper)
}

/// A bare repo for `open_pr`'s `git push` to push into.
///
/// Returned rather than dropped here: `TempDir` removes the directory
/// on drop, so a caller that ignores this gets an `origin` remote
/// pointing at nothing.
#[must_use]
async fn add_bare_origin(repo: &Path) -> TempDir {
    let origin = tempdir();
    git(&origin, &["init", "-q", "--bare"]).await;
    git(
        repo,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    )
    .await;
    origin
}

#[tokio::test]
async fn the_real_coding_task_workflow_walks_the_happy_path_to_done() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;

    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    // The PR comments `awaiting_human_review`'s script reads: an
    // owner's `/approve`. See `owner_comment_page`.
    fs::write(
        scripts_dir.join("verdict"),
        owner_comment_page("looks good\\n/approve"),
    )
    .unwrap();

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

/// One COLLABORATOR review, submitted in 2030 (after the stub's head date).
fn collaborator_review_page(state: &str) -> String {
    format!(
        r#"[{{"id": 5, "state": "{state}", "submitted_at": "2030-01-01T00:00:00Z", "author_association": "COLLABORATOR", "user": {{"login": "rev"}}, "html_url": "https://example.test/r/5", "body": "HUMAN ITEM: from a review"}}]"#
    )
}

/// #230: a GitHub review, not a comment, is the verdict. A
/// `CHANGES_REQUESTED` review routes the real workflow to `revising`.
#[tokio::test]
async fn the_real_coding_task_workflow_revises_on_a_changes_requested_review() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(scripts_dir.join("verdict"), "[]").unwrap();
    fs::write(
        scripts_dir.join("reviews"),
        collaborator_review_page("CHANGES_REQUESTED"),
    )
    .unwrap();

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
    wait_until_stage(&pool, &task_id, "escalate_to_human").await;

    let trail: Vec<String> = stage_trail(&pool, &task_id)
        .await
        .into_iter()
        .map(|(stage, _)| stage)
        .collect();
    assert!(
        trail.iter().filter(|s| s.as_str() == "revising").count() >= 2,
        "a CHANGES_REQUESTED review must route to revising: {trail:?}"
    );
    let payload = payload_of(&pool, &task_id).await;
    let review = payload["stages"]["awaiting_human_review"].as_str().unwrap();
    assert!(review.starts_with("REQUEST_CHANGES\n\n"), "{review:?}");
    assert!(review.contains("review CHANGES_REQUESTED"), "{review:?}");
}

/// #230: an `APPROVED` review alone ends the real workflow as approved.
#[tokio::test]
async fn the_real_coding_task_workflow_finishes_on_an_approved_review() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(scripts_dir.join("verdict"), "[]").unwrap();
    fs::write(
        scripts_dir.join("reviews"),
        collaborator_review_page("APPROVED"),
    )
    .unwrap();

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

    let trail = stage_trail(&pool, &task_id).await;
    assert_eq!(
        trail.last(),
        Some(&("done".to_string(), json!("approved"))),
        "{trail:?}"
    );
}

/// #175: the real `coding-task` at `awaiting_human_review` is answered with
/// `choco task send`: a reply without exactly one marker is refused and
/// changes nothing; a reply with one is the human's review, minus the marker.
#[tokio::test]
async fn the_real_coding_task_review_gate_takes_a_choco_reply() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;

    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    // The PR never reports a verdict: only choco answers.
    fs::write(scripts_dir.join("verdict"), "[]").unwrap();

    let (task_id, def, claude_wrapper) = seed_coding_task(
        &pool,
        &repo,
        &scripts_dir,
        r#"{"outcome": "approved", "feedback": ""}"#,
    )
    .await;
    // Resuming reloads the workflow by name, so seed the shipped ones.
    let workflows_dir = tempdir();
    config_root::seed_builtin_workflows(&workflows_dir).unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &claude_wrapper.to_string_lossy(),
        &workflows_dir,
    );

    engine
        .start_task(&task_id, &def, Some("Add a small feature"))
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "awaiting_human_review").await;
    let trail_before = stage_trail(&pool, &task_id).await.len();

    // No marker: refused, nothing changes.
    let refused = engine.send_message_or_resume(&task_id, "please fix").await;
    assert!(
        matches!(
            refused,
            Err(SendMessageOrResumeError::ReplyNeedsMarker { .. })
        ),
        "{refused:?}"
    );
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "open");
    assert_eq!(
        state_of(&pool, &task_id).await.current_stage,
        "awaiting_human_review"
    );
    assert_eq!(stage_trail(&pool, &task_id).await.len(), trail_before);
    assert!(human_messages(&pool, &task_id).await.is_empty());

    // One marker: the review minus the marker goes to `revising`.
    engine
        .send_message_or_resume(&task_id, "FIX X\n/request-changes")
        .await
        .unwrap();
    let awaiting_entries = |trail: &[(String, Value)]| {
        trail
            .iter()
            .filter(|(stage, _)| stage == "awaiting_human_review")
            .count()
    };
    crate::test_support::wait_until("a second arrival at awaiting_human_review", || async {
        let trail = stage_trail(&pool, &task_id).await;
        if awaiting_entries(&trail) >= 2 {
            Ok(())
        } else {
            Err(format!("trail {trail:?}"))
        }
    })
    .await;
    let trail = stage_trail(&pool, &task_id).await;
    let first = trail
        .iter()
        .position(|(stage, _)| stage == "awaiting_human_review")
        .unwrap();
    assert_eq!(
        trail[first + 1],
        ("revising".to_string(), json!("changes_requested"))
    );
    let payload = payload_of(&pool, &task_id).await;
    assert_eq!(payload["stages"]["awaiting_human_review"], "FIX X");

    let mut render_payload = payload.clone();
    render_payload["arrival"] =
        json!({ "from": "awaiting_human_review", "outcome": "changes_requested" });
    let prompt = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/prompts/coder-revise.md"),
    )
    .unwrap();
    let (rendered, _) = crate::template::render(&prompt, &render_payload).unwrap();
    let from = rendered.find("## The human's review").unwrap();
    let to = rendered.find("## Internal reviewer's summary").unwrap();
    let section = &rendered[from..to];
    assert!(section.contains("FIX X"), "{section}");
    assert!(!section.contains("/request-changes"), "{section}");

    // Approve at the second arrival.
    engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "done").await;
    wait_until_task_status(&pool, &task_id, "closed").await;
}

/// #175, the upgrade: a task the previous daemon left waiting for review
/// (stored `stage_kind` NULL, or `poll`) gets its watcher back with its
/// stored deadline, and its kind corrected so it shows as waiting on a human.
#[tokio::test]
async fn a_task_waiting_for_review_survives_the_upgrade() {
    for stored_kind in [None, Some("poll")] {
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;
        let _origin = add_bare_origin(&repo).await;

        let scripts_dir = tempdir();
        let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
        fs::write(scripts_dir.join("hold"), "").unwrap();
        fs::write(
            scripts_dir.join("verdict"),
            owner_comment_page("looks good\\n/approve"),
        )
        .unwrap();

        let (task_id, _def, claude_wrapper) = seed_coding_task(
            &pool,
            &repo,
            &scripts_dir,
            r#"{"outcome": "approved", "feedback": ""}"#,
        )
        .await;
        // Resuming reloads the workflow by name, so seed the shipped ones.
        let workflows_dir = tempdir();
        config_root::seed_builtin_workflows(&workflows_dir).unwrap();
        let engine = engine_with_adapter_and_workflows_dir(
            pool.clone(),
            &claude_wrapper.to_string_lossy(),
            &workflows_dir,
        );

        let now = Utc::now();
        let deadline = now + chrono::Duration::hours(1);
        let window = window_json(
            "awaiting_human_review",
            now - chrono::Duration::hours(1),
            Some(deadline),
        );
        let stored_deadline = window["deadline"].clone();
        workflow_state::create(
            &pool,
            &task_id,
            "awaiting_human_review",
            "poll",
            json!({
                "task": { "title": "Add a small feature", "input": "x" },
                "poll_window": window,
                "stages": { "open_pr": { "number": 42, "url": "https://example.test/pr/42" } },
            }),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE workflow_state SET stage_kind = ? WHERE task_id = ?")
            .bind(stored_kind)
            .bind(&task_id)
            .execute(&pool)
            .await
            .unwrap();

        // The previous daemon had started the task, so its worktree snapshot
        // is recorded and the directory exists.
        tasks::set_worktree(&pool, &task_id, &repo.to_string_lossy(), "demo")
            .await
            .unwrap();
        fs::create_dir_all(worktree::worktree_path(&repo, "demo", &task_id).unwrap()).unwrap();

        engine.park_interrupted_turns().await.unwrap();
        let report = engine.resume_interrupted_polls().await.unwrap();
        let why = tasks::get(&pool, &task_id)
            .await
            .unwrap()
            .unwrap()
            .stuck_reason;
        assert_eq!(report.resumed, 1, "{stored_kind:?}: {report:?} {why:?}");
        assert_eq!(report.stuck, 0, "{stored_kind:?}: {report:?}");

        // The stub has been called: the watcher is running again.
        crate::test_support::wait_until("the gh stub being held", || async {
            if scripts_dir.join("held").exists() {
                Ok(())
            } else {
                Err("not held yet".to_string())
            }
        })
        .await;

        let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open", "{stored_kind:?}");
        let state = state_of(&pool, &task_id).await;
        assert_eq!(state.stage_kind.as_deref(), Some("human_gate"));
        let summaries = tasks::list_summaries(&pool, None, &[], tasks::SummaryOrder::Id, None)
            .await
            .unwrap();
        let summary = summaries.iter().find(|t| t.task.id == task_id).unwrap();
        assert!(summary.waiting_on_human, "{stored_kind:?}");
        assert_eq!(state.payload["poll_window"]["deadline"], stored_deadline);

        fs::write(scripts_dir.join("release"), "").unwrap();
        wait_until_stage(&pool, &task_id, "done").await;
        wait_until_task_status(&pool, &task_id, "closed").await;
    }
}

/// #175, the upgrade for a task the previous daemon left at
/// `escalate_to_human`, a gate with no watcher: the sweep fills in its kind
/// (which is what keeps it in "Needs you") and starts nothing.
#[tokio::test]
async fn an_escalated_task_keeps_waiting_on_a_human_after_the_upgrade() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let scripts_dir = tempdir();
    let (task_id, _def, claude_wrapper) = seed_coding_task(
        &pool,
        &repo,
        &scripts_dir,
        r#"{"outcome": "approved", "feedback": ""}"#,
    )
    .await;
    // Resuming reloads the workflow by name, so seed the shipped ones.
    let workflows_dir = tempdir();
    config_root::seed_builtin_workflows(&workflows_dir).unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &claude_wrapper.to_string_lossy(),
        &workflows_dir,
    );

    workflow_state::create(
        &pool,
        &task_id,
        "escalate_to_human",
        "human_gate",
        json!({ "task": { "title": "Add a small feature", "input": "x" } }),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();

    engine.park_interrupted_turns().await.unwrap();
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.resumed, 0, "{report:?}");
    assert!(!engine.has_detached_runner(&task_id));

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "open");
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "escalate_to_human");
    assert_eq!(state.stage_kind.as_deref(), Some("human_gate"));
    let summaries = tasks::list_summaries(&pool, None, &[], tasks::SummaryOrder::Id, None)
        .await
        .unwrap();
    let summary = summaries.iter().find(|t| t.task.id == task_id).unwrap();
    assert!(summary.waiting_on_human);
}

/// A planner report carrying the four sections `spec_check` enforces.
fn planner_report(outcome: &str) -> String {
    json!({
            "outcome": outcome,
            "summary": "Checks: all fine\nDecisions: none\nQuestions: none\nSpec: build the small feature",
        })
        .to_string()
}

/// #120: `coding-task-planned` with a planner that reports `ready` walks
/// the whole way to `done`, through `spec_check` first.
#[tokio::test]
async fn the_real_coding_task_planned_workflow_walks_the_happy_path_to_done() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(
        scripts_dir.join("verdict"),
        owner_comment_page("looks good\\n/approve"),
    )
    .unwrap();
    fs::write(
        scripts_dir.join("planner-reply.json"),
        planner_report("ready"),
    )
    .unwrap();

    let (task_id, def, claude_wrapper) = seed_builtin_coding_task(
        &pool,
        &repo,
        &scripts_dir,
        r#"{"outcome": "approved", "feedback": ""}"#,
        "coding-task-planned",
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
            "spec_check",
            "coding",
            "internal_review",
            "open_pr",
            "checks_polling",
            "awaiting_human_review",
            "done",
        ]
    );
}

/// #241: an owner's `/request-changes` review with inline comments, on the
/// real `coding-task-planned`, routes to `revising` and hands the coder
/// every inline comment. (The mock coder leaves `revising` at once, so the
/// test waits for the loop guard to park the task and reads the trail.)
#[tokio::test]
async fn the_real_planned_workflow_hands_an_owners_inline_comments_to_the_coder() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(scripts_dir.join("verdict"), "[]").unwrap();
    fs::write(
        scripts_dir.join("reviews"),
        r#"[{"id": 5, "state": "COMMENTED", "submitted_at": "2030-01-01T00:00:00Z", "author_association": "OWNER", "user": {"login": "owner"}, "html_url": "https://example.test/r/5", "body": "note\n/request-changes"}]"#,
    )
    .unwrap();
    let inline = |path: &str, line: u32, body: &str| {
        json!({
            "pull_request_review_id": 5, "author_association": "OWNER",
            "user": {"login": "owner"}, "path": path, "line": line,
            "original_line": line, "subject_type": "line",
            "html_url": format!("https://example.test/i/{line}"), "body": body,
        })
    };
    fs::write(
        scripts_dir.join("review-comments"),
        json!([
            inline("src/a.rs", 5, "INLINE-ENGINE-ONE"),
            inline("src/b.rs", 9, "INLINE-ENGINE-TWO")
        ])
        .to_string(),
    )
    .unwrap();
    fs::write(
        scripts_dir.join("planner-reply.json"),
        planner_report("ready"),
    )
    .unwrap();

    let (task_id, def, claude_wrapper) = seed_builtin_coding_task(
        &pool,
        &repo,
        &scripts_dir,
        r#"{"outcome": "approved", "feedback": ""}"#,
        "coding-task-planned",
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
    assert!(
        trail.iter().filter(|s| s.as_str() == "revising").count() >= 2,
        "the owner's request must route to revising: {trail:?}"
    );
    let payload = payload_of(&pool, &task_id).await;
    let review = payload["stages"]["awaiting_human_review"].as_str().unwrap();
    assert!(review.starts_with("REQUEST_CHANGES\n\n"), "{review:?}");
    assert!(review.contains("review COMMENTED"), "{review:?}");
    for body in ["INLINE-ENGINE-ONE", "INLINE-ENGINE-TWO"] {
        assert!(review.contains(body), "{body}: {review:?}");
    }
}

/// #120: a planner that reports `needs_input` parks the task, open, at
/// `spec_questions`; the human's answer goes back to `spec_check`, and
/// the second report (`ready`) lets the coder start.
#[tokio::test]
async fn the_planned_workflow_parks_for_answers_then_resumes_through_spec_check() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(
        scripts_dir.join("verdict"),
        owner_comment_page("looks good\\n/approve"),
    )
    .unwrap();
    fs::write(
        scripts_dir.join("planner-reply-1.json"),
        planner_report("needs_input"),
    )
    .unwrap();
    fs::write(
        scripts_dir.join("planner-reply.json"),
        planner_report("ready"),
    )
    .unwrap();

    let (task_id, def, claude_wrapper) = seed_builtin_coding_task(
        &pool,
        &repo,
        &scripts_dir,
        r#"{"outcome": "approved", "feedback": ""}"#,
        "coding-task-planned",
    )
    .await;
    // The resume reloads the workflow by name, so the engine needs the
    // shipped workflows seeded where it looks for them.
    let workflows_dir = tempdir();
    config_root::seed_builtin_workflows(&workflows_dir).unwrap();
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &claude_wrapper.to_string_lossy(),
        &workflows_dir,
    );

    engine
        .start_task(&task_id, &def, Some("Add a small feature"))
        .await
        .unwrap();
    wait_until_stage(&pool, &task_id, "spec_questions").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "open");

    engine
        .send_message_or_resume(&task_id, "ANSWER")
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
            "spec_check",
            "spec_questions",
            "spec_check",
            "coding",
            "internal_review",
            "open_pr",
            "checks_polling",
            "awaiting_human_review",
            "done",
        ]
    );
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.payload["stages"]["spec_questions"], "ANSWER");
}

/// A merged PR counts as approval (#102): `MERGED` routes
/// `awaiting_human_review` to `done` through the same `approved` edge as
/// `/approve`, and wins over a standing `/request-changes`.
#[tokio::test]
async fn the_real_coding_task_workflow_treats_a_merged_pr_as_approval() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(scripts_dir.join("pr-merged"), "").unwrap();
    fs::write(
        scripts_dir.join("verdict"),
        owner_comment_page("not yet\\n/request-changes"),
    )
    .unwrap();

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

    let trail = stage_trail(&pool, &task_id).await;
    let n = trail.len();
    assert_eq!(trail[n - 2].0, "awaiting_human_review");
    assert_eq!(trail[n - 1], ("done".to_string(), json!("approved")));
    assert!(
        !trail.iter().any(|(stage, _)| stage == "revising"),
        "a merged PR must not be sent back for revision: {trail:?}"
    );
}

/// A `/request-changes` verdict routes back through `revising` *and*
/// survives the return trip through `open_pr` (#78).
///
/// Both halves of #78 meet here and neither is provable alone. The
/// verdict half: `awaiting_human_review` reads a marker line out of a
/// PR comment, so a route to `revising` exists at all — before the fix
/// it polled `reviewDecision`, which nothing on a solo repo can set.
/// The `open_pr` half: that route immediately re-enters `open_pr`,
/// where the old unconditional `gh pr create` failed on "a PR already
/// exists" and diverted the task into `escalate_to_human` — so the
/// changes-requested edge was unreachable *twice over*, and fixing
/// only the poll would have swapped one dead end for another.
///
/// The verdict file never changes, so every lap requests changes
/// again and the task ends where it should: parked at the loop guard,
/// not spinning. `pr create` having run exactly once is the assertion
/// that pins the second-lap fix specifically.
#[tokio::test]
async fn the_real_coding_task_workflow_reopens_the_same_pr_when_changes_are_requested() {
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let _origin = add_bare_origin(&repo).await;
    let scripts_dir = tempdir();
    let _path_guard = PathPrefixGuard::new(&gh_stub_dir(&scripts_dir));
    fs::write(
        scripts_dir.join("verdict"),
        owner_comment_page("HUMAN ITEM: fix the release build\\n/request-changes"),
    )
    .unwrap();

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
    wait_until_stage(&pool, &task_id, "escalate_to_human").await;

    let raw_trail = stage_trail(&pool, &task_id).await;
    let trail: Vec<String> = raw_trail.iter().map(|(stage, _)| stage.clone()).collect();
    assert!(
        trail.iter().filter(|s| s.as_str() == "open_pr").count() >= 2,
        "the changes-requested route must come back through open_pr at \
             least once more: {trail:?}"
    );
    assert!(
        trail.iter().filter(|s| s.as_str() == "revising").count() >= 2,
        "a /request-changes verdict must route to revising: {trail:?}"
    );
    assert_eq!(
        trail.iter().filter(|s| s.as_str() == "coding").count(),
        1,
        "coding still only ever runs once: {trail:?}"
    );
    // #106: `awaiting_human_review`'s own `loop_guard` (max: 3) is what
    // parks this task, not `internal_review`'s (the coder's stub reply
    // always approves) — so it must appear exactly 4 times: the first
    // visit plus the 3 allowed `changes_requested` laps, with the 4th
    // rerouting to `escalate_to_human` instead of coming back around.
    assert_eq!(
        trail
            .iter()
            .filter(|s| s.as_str() == "awaiting_human_review")
            .count(),
        4,
        "expected exactly 4 visits to awaiting_human_review: {trail:?}"
    );
    let last = raw_trail.last().expect("trail is never empty");
    assert_eq!(
        last,
        &("escalate_to_human".to_string(), json!("changes_requested")),
        "the last hop must be awaiting_human_review's guard tripping \
             on changes_requested: {raw_trail:?}"
    );
    let last_awaiting_review_index = raw_trail
        .iter()
        .rposition(|(stage, _)| stage == "awaiting_human_review")
        .expect("awaiting_human_review must appear in the trail");
    assert_eq!(
        last_awaiting_review_index,
        raw_trail.len() - 2,
        "escalate_to_human must be entered immediately after the last \
             awaiting_human_review, not via some other stage: {raw_trail:?}"
    );

    // #138: the poll's capture is the human's review, and it is in the
    // payload, here on the path where the poll's own loop guard routed
    // the 4th `changes_requested` to `escalate_to_human`.
    let payload = payload_of(&pool, &task_id).await;
    let review = payload["stages"]["awaiting_human_review"]
        .as_str()
        .unwrap_or_else(|| panic!("no text capture in {payload}"));
    assert!(
        review.starts_with("REQUEST_CHANGES\n\n### owner (OWNER), "),
        "{review:?}"
    );
    assert!(review.contains("HUMAN ITEM: fix the release build"));
    // ...and it renders into the coder's prompt under its heading.
    let prompt_src = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/prompts/coder-revise.md"),
    )
    .unwrap();
    let mut render_payload = payload.clone();
    render_payload["arrival"] =
        json!({ "from": "awaiting_human_review", "outcome": "changes_requested" });
    let (prompt, _) = crate::template::render(&prompt_src, &render_payload).unwrap();
    let heading = prompt.find("## The human's review").unwrap();
    let item = prompt.find("HUMAN ITEM: fix the release build").unwrap();
    assert!(heading < item, "{prompt}");

    // The load-bearing one: a second `gh pr create` is exactly the
    // failure #78's second half describes, and it would have shown up
    // above only as an early `escalate_to_human` that looks like a
    // tripped loop guard.
    let creates = fs::read_to_string(scripts_dir.join("pr-created")).unwrap();
    assert_eq!(
        creates.lines().count(),
        1,
        "gh pr create must run once and later laps reuse the open PR: {creates:?}"
    );
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
    // #106 (must fail on main): `internal_review` is entered from
    // `coding` once and from `revising` every time after — exactly the
    // shape whose reset the old entry-based rule forgot the first
    // rejection for, letting the task escalate on the 5th rejection
    // (4 `revising` trips, 5 `internal_review` entries) instead of the
    // 4th.
    assert_eq!(
        trail.iter().filter(|s| s.as_str() == "revising").count(),
        3,
        "expected exactly 3 trips through revising before the loop guard tripped: {trail:?}"
    );
    assert_eq!(
        trail
            .iter()
            .filter(|s| s.as_str() == "internal_review")
            .count(),
        4,
        "expected exactly 4 visits to internal_review before the loop guard tripped: {trail:?}"
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
        },
    )
    .await
    .unwrap()
    .id;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();

    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
    wait_until_events_contain_prefix(
        &pool,
        &run.id,
        "model=sonnet|system_prompt=<unset>|permission_mode=bypassPermissions|",
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
    wait_until_events_contain_prefix(
        &pool,
        &run.id,
        "model=sonnet|system_prompt=<unset>|permission_mode=<unset>|",
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

    engine.cancel_task(&task_id, false).await.unwrap();

    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "cancelled"
    );
    crate::test_support::wait_until(
        &format!("session {} to be recorded as cancelled", run.id),
        || async {
            let run = sessions::get(&pool, &run.id).await.unwrap().unwrap();
            if run.end_reason == Some(SessionEndReason::Cancelled) {
                Ok(())
            } else {
                Err(format!(
                    "status {:?}, end_reason {:?}",
                    run.status, run.end_reason
                ))
            }
        },
    )
    .await;
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

    engine.cancel_task(&task_id, false).await.unwrap();

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

    engine.cancel_task(&task_id, false).await.unwrap();

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
    engine.cancel_task(&task_id, false).await.unwrap();

    let err = engine.advance(&task_id, &def, "resumed").await.unwrap_err();
    assert!(matches!(err, EngineError::TaskCancelled(_)));

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "gate");
}

/// The hole this closes: `send_message_or_resume` dispatches on stage
/// *kind*, which cancel deliberately doesn't change. Without the
/// `tasks.status` check, resuming this `human_gate` would advance a
/// cancelled task; for a standing-open `agent_turn` it would go further
/// and spawn a fresh subprocess from the persisted `adapter_session_id`,
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
    engine.cancel_task(&task_id, false).await.unwrap();

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
    engine.cancel_task(&task_id, false).await.unwrap();

    let err = engine.cancel_task(&task_id, false).await.unwrap_err();
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

    let err = engine.cancel_task(&task_id, false).await.unwrap_err();
    assert!(matches!(err, CancelTaskError::NotCancellable(status) if status == "closed"));
}

#[tokio::test]
async fn cancelling_an_unknown_task_is_rejected() {
    let pool = connect_in_memory().await.unwrap();
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

    let err = engine.cancel_task("no-such-task", false).await.unwrap_err();
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
        handles.push(tokio::spawn(async move {
            engine.cancel_task(&task_id, false).await
        }));
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

    engine.cancel_task(&task_id, false).await.unwrap();

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
/// `is_cancelled` check in `run_watch` (which ends it at the next
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
    crate::test_support::wait_until("the poll command to append to the marker file", || async {
        let len = fs::metadata(&marker).map(|m| m.len());
        if len.as_ref().is_ok_and(|len| *len >= 1) {
            Ok(())
        } else {
            Err(format!("marker length {len:?}"))
        }
    })
    .await;
    assert!(fs::metadata(&marker).is_ok(), "the poll never ran at all");

    engine.cancel_task(&task_id, false).await.unwrap();
    let at_cancel = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);

    // Several intervals' worth: a still-running loop would add attempts.
    tokio::time::sleep(StdDuration::from_millis(2500)).await;
    let later = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        at_cancel, later,
        "a cancelled task kept polling: {at_cancel} attempts at cancel, {later} after"
    );
}

/// A `shell` stage owns no `session`, so killing the task's agent
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
    command: "for i in $(seq 1 2000); do printf x >> {} ; sleep 0.05; done"
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
    crate::test_support::wait_until(
        "the shell command to tick twice into the marker file",
        || async {
            let len = fs::metadata(&marker).map(|m| m.len());
            if len.as_ref().is_ok_and(|len| *len >= 2) {
                Ok(())
            } else {
                Err(format!("marker length {len:?}"))
            }
        },
    )
    .await;
    assert!(
        fs::metadata(&marker).is_ok(),
        "the shell command never started"
    );

    engine.cancel_task(&task_id, false).await.unwrap();
    // `cancel_task` awaits the aborted runner, so the process group is
    // already killed here. One tick of slack absorbs a `printf` that was
    // in flight; any growth after that means the command survived.
    tokio::time::sleep(StdDuration::from_millis(60)).await;
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

    crate::test_support::wait_until(
        "a finished shell runner's registry entry to be removed",
        || async {
            let left = engine
                .detached_runners
                .lock()
                .expect("detached_runners mutex poisoned")
                .len();
            if left == 0 {
                Ok(())
            } else {
                Err(format!("{left} runner entries left"))
            }
        },
    )
    .await;
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
    engine.cancel_task(&task_id, false).await.unwrap();

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

/// A task cancelled before it ever reached a stage that called
/// `worktree::ensure` has no snapshot, so cancel's worktree step has
/// nothing to remove and must not fail the cancel.
///
/// This pins the *outcome*, not the skip: `remove_worktree` only logs
/// and returns when there is no snapshot, so deleting the
/// `worktree_snapshot` gate would leave this test passing. The gate
/// exists to keep that error log out of a perfectly ordinary cancel,
/// which isn't worth asserting on a log line to prove.
#[tokio::test]
async fn cancel_succeeds_for_a_task_that_never_made_a_worktree() {
    let pool = connect_in_memory().await.unwrap();
    let def = human_gate_chain_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));

    engine.start_task(&task_id, &def, None).await.unwrap();
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(worktree_snapshot(&task).is_none());

    engine.cancel_task(&task_id, false).await.unwrap();

    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "cancelled"
    );
}

// ---- X-4: stuck tasks and retry (#61) ----

/// A single shell stage gated on a marker file: `on: { done: finished }`
/// with *no* `error` edge, so a missing marker leaves the command's
/// `error` outcome nowhere to route and the task is marked stuck.
/// Creating the marker and retrying lets the same command succeed.
///
/// Written to `workflows_dir` as `retry-flow.yaml`, not just parsed in
/// memory: `retry_task` reloads the task's workflow via
/// `load_task_workflow` — the recorded `workflow_path` for a task
/// created through `create_task`, or a name lookup for a legacy one —
/// so a test driving it needs a real file on disk — unlike `start_task`,
/// which takes an already-loaded definition directly and never touches
/// `workflows_dir` itself.
fn write_marker_shell_workflow(workflows_dir: &Path, marker: &Path) -> Arc<WorkflowDefinition> {
    let yaml = format!(
        r#"
name: retry-flow
stages:
  run:
    kind: shell
    command: "test -f {}"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
        marker.display()
    );
    std::fs::write(workflows_dir.join("retry-flow.yaml"), &yaml).unwrap();
    Arc::new(WorkflowDefinition::parse(&yaml, workflows_dir).unwrap())
}

#[tokio::test]
async fn a_session_start_failure_while_advancing_into_an_agent_turn_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
    let yaml = r#"
name: shell-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  run:
    kind: shell
    command: "true"
    on: { done: coding }
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
    // A binary that can't be spawned at all, so entering `coding` from
    // `run` fails synchronously inside `advance_from_stage` — the same
    // technique
    // `a_failed_session_start_marks_the_session_exited_instead_of_wedging_it`
    // uses.
    let engine = engine_with_adapter(pool.clone(), "/no/such/binary-3f6c9a");

    engine.start_task(&task_id, &def, None).await.unwrap();

    wait_until_task_status(&pool, &task_id, "stuck").await;
    // `advance_from_stage` writes `workflow_state.current_stage` to the
    // new stage *before* `enter_stage` (and the session start inside
    // it) runs, so this is already `coding` — the stage `retry_task`
    // would re-enter.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(
        state.current_stage, "coding",
        "workflow_state must already name the stage that failed to start"
    );
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    // The reason names `coding` — the stage that actually failed to
    // start — not `run`, which completed fine (review, X-4 round 2).
    // `workflow_state.current_stage` already reads `coding` by the time
    // `finish_detached`'s catch-all runs, so that's also the stage
    // `retry_task` would re-enter; naming `run` here would point a
    // human (and a retry) at the wrong place.
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("coding")
                && r.contains("run")
                && r.contains("could not be entered")),
        "{:?}",
        task.stuck_reason
    );
}

#[tokio::test]
async fn create_task_whose_entry_agent_turn_cannot_start_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    std::fs::write(
        workflows_dir.join("broken.yaml"),
        r#"
name: broken
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        "/no/such/binary-3f6c9a",
        &workflows_dir,
    );

    let err = engine
        .create_task(&project_id, "broken", "t", "hello", json!({}))
        .await
        .unwrap_err();

    let tasks = tasks::list(&pool, Some(&project_id), None).await.unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "create_task must still have written the task row"
    );
    // Review, X-4 round 2: the caller never sees the `Task` `start_task`
    // was trying to start, only this `Err` — so `CreateTaskError::Start`
    // must carry the id itself, or there'd be no way to find the
    // now-`stuck` row from the error alone.
    match &err {
        CreateTaskError::Start { task_id, .. } => assert_eq!(task_id, &tasks[0].id),
        other => panic!("expected CreateTaskError::Start, got {other:?}"),
    }
    assert_eq!(tasks[0].status, "stuck");
    assert!(tasks[0].stuck_reason.is_some());
}

#[tokio::test]
async fn stage_entered_events_carry_the_stage_kind() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker");
    std::fs::write(&marker, "").unwrap();
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;

    let kinds: Vec<(String, Value)> = events::list_stage_trail(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| {
            (
                e.payload["stage"].as_str().unwrap().to_string(),
                e.payload["kind"].clone(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("run".to_string(), json!("shell")),
            ("finished".to_string(), json!("terminal")),
        ]
    );
}

#[tokio::test]
async fn retry_task_reruns_a_stuck_shell_stage_and_it_can_succeed() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker");
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    std::fs::write(&marker, "").unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();

    wait_until_task_status(&pool, &task_id, "closed").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.stuck_reason, None);

    let trail = stage_trail(&pool, &task_id).await;
    assert!(
        trail
            .iter()
            .any(|(stage, outcome)| stage == "run" && outcome == &json!("retry")),
        "expected a stage_entered event with outcome 'retry': {trail:?}"
    );
}

/// `retry_task` on a stuck task created from a repo workflow (issue
/// #88) reloads the recorded path, not a fresh by-name lookup — proved
/// the same way the `send_message_or_resume` version above is: a
/// different, broken `retry-flow.yaml` lands in the global directory
/// after the task got stuck, and the retry still succeeds against the
/// repo file.
#[tokio::test]
async fn retry_task_on_a_repo_workflow_task_reloads_the_recorded_path() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    let marker = repo_dir.join("marker");
    let yaml = format!(
        r#"
name: retry-flow
stages:
  run:
    kind: shell
    command: "test -f {}"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
        marker.display()
    );
    write_repo_workflow(&repo_dir, "retry-flow", &yaml);
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &global_dir);

    let task = engine
        .create_task(&project.id, "retry-flow", "t", "hi", json!({}))
        .await
        .unwrap();
    wait_until_task_status(&pool, &task.id, "stuck").await;

    // A different, broken `retry-flow.yaml` (no `run` stage at all)
    // lands in the global directory only now — if `retry_task` fell
    // back to a name lookup it would load this one and fail with
    // `UnknownStage` instead of re-running the shell command.
    std::fs::write(
        global_dir.join("retry-flow.yaml"),
        "name: retry-flow\nstages:\n  other_stage:\n    kind: terminal\n",
    )
    .unwrap();

    std::fs::write(&marker, "").unwrap();
    engine.retry_task(&task.id, RetryMode::Auto).await.unwrap();

    wait_until_task_status(&pool, &task.id, "closed").await;
}

// ---- init_project_workflows (issue #88) ----

#[tokio::test]
async fn init_project_workflows_seeds_the_builtins_and_never_overwrites() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    let project = projects::create(&pool, "demo", Some(&repo_dir.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &global_dir);

    let report = engine.init_project_workflows(&project.id).await.unwrap();
    let target = repo_dir.join(".chocofactory").join("workflows");
    assert!(target.join("coding-task.yaml").is_file());
    assert!(target.join("chat.yaml").is_file());
    for name in [
        "coder-system.md",
        "coder-turn.md",
        "coder-revise.md",
        "reviewer-system.md",
        "reviewer-turn.md",
    ] {
        assert!(
            target.join("prompts").join(name).is_file(),
            "expected prompts/{name} to be seeded"
        );
    }
    assert!(
        report.created.contains(&target.join("chat.yaml")),
        "{report:?}"
    );

    // A user edits one of the seeded files...
    std::fs::write(target.join("chat.yaml"), "name: my-custom-chat\n").unwrap();

    // ...and a second call reports everything as already existing,
    // leaving the edit untouched.
    let second = engine.init_project_workflows(&project.id).await.unwrap();
    assert!(second.created.is_empty(), "{second:?}");
    assert!(!second.existing.is_empty(), "{second:?}");
    assert_eq!(
        std::fs::read_to_string(target.join("chat.yaml")).unwrap(),
        "name: my-custom-chat\n"
    );
}

#[tokio::test]
async fn init_project_workflows_without_a_repo_path_errors() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let project = projects::create(&pool, "demo", None).await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &global_dir);

    let err = engine
        .init_project_workflows(&project.id)
        .await
        .unwrap_err();
    assert!(matches!(err, InitWorkflowsError::NoRepoPath(id) if id == project.id));
}

#[tokio::test]
async fn init_project_workflows_with_a_missing_repo_path_errors() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let repo_dir = tempdir();
    let missing = repo_dir.join("does-not-exist");
    let project = projects::create(&pool, "demo", Some(&missing.to_string_lossy()))
        .await
        .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &global_dir);

    let err = engine
        .init_project_workflows(&project.id)
        .await
        .unwrap_err();
    assert!(matches!(err, InitWorkflowsError::RepoPathMissing(path) if path == missing));
}

#[tokio::test]
async fn init_project_workflows_on_an_unknown_project_errors() {
    let pool = connect_in_memory().await.unwrap();
    let global_dir = tempdir();
    let engine = engine_with_adapter_and_workflows_dir(pool, "unused", &global_dir);

    let err = engine
        .init_project_workflows("no-such-project")
        .await
        .unwrap_err();
    assert!(matches!(err, InitWorkflowsError::NoSuchProject(id) if id == "no-such-project"));
}

#[tokio::test]
async fn retry_task_that_fails_again_re_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();

    // The reopen lands immediately; the re-run of the same failing
    // command lands the task back on stuck a moment later.
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(task.stuck_reason.is_some());
}

#[tokio::test]
async fn retry_task_reruns_a_stuck_agent_turn_stage_and_it_can_succeed() {
    // Review, X-4 round 2: every other retry test drives a `shell`
    // stage; this is the case the issue names first — a stuck
    // `agent_turn` whose session failed to start.
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "do the thing").unwrap();
    let yaml = r#"
name: shell-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  run:
    kind: shell
    command: "true"
    on: { done: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
    // `retry_task` re-resolves the workflow from `workflows_dir` by
    // name (unlike `start_task`, which is handed the already-parsed
    // `def`), so the file has to actually be on disk for it to find.
    std::fs::write(dir.join("shell-then-turn.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;

    // A binary that can't be spawned at all, so `coding` never starts.
    let broken_engine =
        engine_with_adapter_and_workflows_dir(pool.clone(), "/no/such/binary-3f6c9a", &dir);
    broken_engine
        .start_task(&task_id, &def, None)
        .await
        .unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;
    assert_eq!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .unwrap()
            .current_stage,
        "coding"
    );

    // `retry_task` doesn't have to come from the same engine instance
    // that got the task stuck — only the pool and workflows_dir need to
    // agree — so this models "the operator fixed whatever was wrong"
    // (here: a working binary) between the failure and the retry.
    let fixed_engine =
        engine_with_adapter_and_workflows_dir(pool.clone(), &reply_binary(&dir, "ok"), &dir);
    fixed_engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap();

    wait_until_task_status(&pool, &task_id, "closed").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.stuck_reason, None);

    let trail = stage_trail(&pool, &task_id).await;
    assert!(
        trail
            .iter()
            .any(|(stage, outcome)| stage == "coding" && outcome == &json!("retry")),
        "expected a stage_entered event with outcome 'retry': {trail:?}"
    );
    let runs: Vec<_> = sessions::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.stage == "coding")
        .collect();
    assert_eq!(
        runs.len(),
        2,
        "expected the failed session-start attempt's session and the retry's own"
    );
}

// ---- #172: read-only roles ----

struct ReadOnlyRun {
    pool: SqlitePool,
    dir: TempDir,
    _repo: TempDir,
    task_id: String,
    worktree_dir: PathBuf,
    def: Arc<WorkflowDefinition>,
    _engine: Arc<WorkflowEngine>,
}

const READ_ONLY_WORKFLOW: &str = r#"
name: ro-flow
worktree: true
roles:
  reviewer:
    cli: claude
    model: opus
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
  coder:
    cli: claude
    model: sonnet
stages:
  prep:
    kind: shell
    command: "PREP"
    on: { done: review, error: verified }
  review:
    kind: agent_turn
    role: ROLE
    prompt_file: p.md
    on: { done: verified }
  verified:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;

/// Creates a task on `ro-flow` (entry stage `review`, or `prep` when
/// `with_prep`) in a real repo, and starts it with `steps` as the agent.
async fn start_read_only_task(
    role: &str,
    with_prep: bool,
    task_config: Value,
    steps: Value,
) -> ReadOnlyRun {
    start_read_only_task_with_sql(role, with_prep, task_config, steps, None).await
}

/// As `start_read_only_task`, running `setup_sql` on the pool first.
async fn start_read_only_task_with_sql(
    role: &str,
    with_prep: bool,
    task_config: Value,
    steps: Value,
    setup_sql: Option<&str>,
) -> ReadOnlyRun {
    start_read_only_task_full(role, with_prep, task_config, steps, setup_sql, None).await
}

/// As `start_read_only_task_with_sql`, with fast turn timers when given.
async fn start_read_only_task_full(
    role: &str,
    with_prep: bool,
    task_config: Value,
    steps: Value,
    setup_sql: Option<&str>,
    timers: Option<crate::session::TurnTimers>,
) -> ReadOnlyRun {
    start_read_only_task_prepped(
        role,
        with_prep,
        "printf 'gitdir: /nonexistent\\n' > .git",
        task_config,
        steps,
        setup_sql,
        timers,
    )
    .await
}

/// As `start_read_only_task_full`, with the `prep` stage's shell command.
async fn start_read_only_task_prepped(
    role: &str,
    with_prep: bool,
    prep: &str,
    task_config: Value,
    steps: Value,
    setup_sql: Option<&str>,
    timers: Option<crate::session::TurnTimers>,
) -> ReadOnlyRun {
    let pool = connect_in_memory().await.unwrap();
    if let Some(sql) = setup_sql {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap();
    }
    let dir = tempdir();
    let repo = tempdir();
    init_git_repo(&repo).await;
    fs::write(dir.join("p.md"), "review it").unwrap();
    let mut yaml = READ_ONLY_WORKFLOW
        .replace("ROLE", role)
        .replace("PREP", prep);
    if !with_prep {
        // Drop the `prep` stage so `review` is the entry stage.
        let start = yaml.find("  prep:").unwrap();
        let end = yaml.find("  review:").unwrap();
        yaml.replace_range(start..end, "");
    }
    fs::write(dir.join("ro-flow.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let mut config = json!({ "cwd": repo.to_string_lossy() });
    if let Some(extra) = task_config.as_object() {
        for (k, v) in extra {
            config[k] = v.clone();
        }
    }
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = tasks::create(
        &pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: &def.name,
            title: "T",
            config,
            workflow_path: None,
            workflow_sha256: None,
        },
    )
    .await
    .unwrap()
    .id;
    let binary = named_script_binary(&dir, "fake-claude-ro", steps);
    let engine = match timers {
        Some(timers) => engine_with_turn_timers(pool.clone(), &binary, timers),
        None => engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir),
    };
    let started = engine.start_task(&task_id, &def, None).await;
    if setup_sql.is_none() {
        started.unwrap();
    } else if let Err(err) = started {
        // An entry-stage failure is returned to the caller as well as
        // parking the task; the tests read the parked state.
        eprintln!("start_task: {err}");
    }
    let worktree_dir = worktree::worktree_path(&repo, "demo", &task_id).unwrap();
    ReadOnlyRun {
        pool,
        dir,
        _repo: repo,
        task_id,
        worktree_dir,
        def,
        _engine: engine,
    }
}

fn ro_steps(commands: &[&str]) -> Value {
    let mut steps = vec![json!({"op": "read_turn"})];
    for command in commands {
        steps.push(json!({"op": "run", "command": command}));
    }
    steps.push(json!({"op": "report", "outcome": "done"}));
    steps.push(json!({"op": "result"}));
    Value::Array(steps)
}

async fn session_events(pool: &SqlitePool, session: &Session, kind: EventType) -> Vec<Value> {
    events::list_for_session(pool, &session.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == kind)
        .map(|e| e.payload)
        .collect()
}

async fn stuck_reason(pool: &SqlitePool, task_id: &str) -> String {
    wait_until_task_status(pool, task_id, "stuck").await;
    tasks::get(pool, task_id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap()
}

#[tokio::test]
async fn a_read_only_turn_that_changes_nothing_advances() {
    let run = start_read_only_task("reviewer", false, json!({}), ro_steps(&[])).await;
    wait_until_stage(&run.pool, &run.task_id, "verified").await;

    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    let baselines = session_events(&run.pool, &session, EventType::WorktreeBaseline).await;
    assert_eq!(baselines.len(), 1);
    let head = worktree::snapshot(&run.worktree_dir).await.unwrap().head;
    assert_eq!(baselines[0]["head"], json!(head));
    assert!(baselines[0]["inherited_from"].is_null());
    assert_eq!(baselines[0]["role"], "reviewer");
    assert!(
        session_events(&run.pool, &session, EventType::WorktreeChanged)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_read_only_turn_that_adds_a_file_parks_the_task() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["touch sneaky.txt"]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("'reviewer'")
            && reason.contains("'review'")
            && reason.contains("git status changed (1 entries)"),
        "{reason}"
    );
    assert!(run.worktree_dir.join("sneaky.txt").exists());
    let state = workflow_state::get(&run.pool, &run.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.current_stage, "review");
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert!(
        session_events(&run.pool, &session, EventType::TurnOutcome)
            .await
            .is_empty()
    );
    let changed = session_events(&run.pool, &session, EventType::WorktreeChanged).await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["message"], json!(reason));
    assert_eq!(changed[0]["changes"][0]["field"], "status");
    assert_eq!(changed[0]["status"], json!(["?? sneaky.txt"]));
}

#[tokio::test]
async fn a_read_only_turn_that_commits_parks_the_task() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["git commit -q --allow-empty -m sneaky"]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    let baselines = session_events(&run.pool, &session, EventType::WorktreeBaseline).await;
    let old = baselines[0]["head"].as_str().unwrap().to_string();
    let new = worktree::snapshot(&run.worktree_dir).await.unwrap().head;
    assert!(
        reason.contains(&format!("HEAD {} → {}", &old[..7], &new[..7])),
        "{reason}"
    );
}

/// A turn that crashes after changing the worktree is still checked, so
/// a plain retry (a fresh session) can't take the change as its baseline.
#[tokio::test]
async fn a_read_only_turn_that_crashes_after_changing_the_worktree_is_caught() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt && false"},
        ]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("exited without completing")
            && reason.contains("changed the worktree")
            && reason.contains("git status changed (1 entries)"),
        "{reason}"
    );
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    let changed = session_events(&run.pool, &session, EventType::WorktreeChanged).await;
    assert_eq!(changed.len(), 1);
}

#[tokio::test]
async fn a_read_only_turn_that_ends_without_a_report_is_still_checked() {
    let run = start_read_only_task_full(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "git commit -q --allow-empty -m sneaky"},
            {"op": "result"},
            {"op": "answer_every_turn", "text": "still waiting"},
        ]),
        None,
        Some(fast_turn_timers()),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("without calling report_outcome") && reason.contains("HEAD "),
        "{reason}"
    );
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert_eq!(
        session_events(&run.pool, &session, EventType::WorktreeChanged)
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn an_abnormal_read_only_turn_that_changed_nothing_keeps_its_own_reason() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([{"op": "read_turn"}, {"op": "run", "command": "false"}]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(!reason.contains("changed the worktree"), "{reason}");
    assert!(!reason.contains("could not verify"), "{reason}");
}

#[tokio::test]
async fn a_read_only_turn_that_restores_what_it_touched_advances() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["printf changed > README.md && git checkout -q -- README.md"]),
    )
    .await;
    wait_until_stage(&run.pool, &run.task_id, "verified").await;
}

#[tokio::test]
async fn task_config_cannot_loosen_a_read_only_role() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({"roles": {"reviewer": {"read_only": false, "disallowed_tools": []}}}),
        ro_steps(&["touch sneaky.txt"]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(reason.contains("changed the worktree"), "{reason}");
}

#[tokio::test]
async fn a_resumed_read_only_turn_is_checked_against_the_original_baseline() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt"},
            {"op": "usage_limit"},
        ]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(reason.contains("interrupted"), "{reason}");
    assert!(reason.contains("changed the worktree"), "{reason}");
    let first = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert_eq!(
        session_events(&run.pool, &first, EventType::WorktreeChanged)
            .await
            .len(),
        1
    );
    let first_baseline = session_events(&run.pool, &first, EventType::WorktreeBaseline)
        .await
        .remove(0);

    let binary = named_script_binary(&run.dir, "fake-claude-ro-resumed", ro_steps(&[]));
    let engine = engine_with_adapter_and_workflows_dir(run.pool.clone(), &binary, &run.dir);
    let outcome = engine
        .retry_task(&run.task_id, RetryMode::Auto)
        .await
        .unwrap();
    assert!(outcome.resumed);
    wait_until_task_status(&run.pool, &run.task_id, "stuck").await;
    let mut reason = String::new();
    for _ in 0..200 {
        reason = stuck_reason(&run.pool, &run.task_id).await;
        if reason.contains("changed the worktree") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(reason.contains("changed the worktree"), "{reason}");
    assert!(reason.contains("git status changed"), "{reason}");

    let second = run_after(&run.pool, &run.task_id, "review", &first).await;
    let baseline = session_events(&run.pool, &second, EventType::WorktreeBaseline)
        .await
        .remove(0);
    assert_eq!(baseline["inherited_from"], json!(first.id));
    assert_eq!(baseline["head"], first_baseline["head"]);
    assert_eq!(baseline["status_sha256"], first_baseline["status_sha256"]);
}

#[tokio::test]
async fn the_restart_sweep_checks_a_read_only_turn_it_strands() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt"},
            {"op": "usage_limit"},
        ]),
    )
    .await;
    stuck_reason(&run.pool, &run.task_id).await;
    // Put the task back as a daemon crash would have left it.
    sqlx::query("UPDATE tasks SET status = 'open', stuck_reason = NULL WHERE id = ?")
        .bind(&run.task_id)
        .execute(&run.pool)
        .await
        .unwrap();
    let binary = named_script_binary(&run.dir, "fake-claude-ro-sweep", ro_steps(&[]));
    let engine = engine_with_adapter_and_workflows_dir(run.pool.clone(), &binary, &run.dir);
    engine.park_interrupted_turns().await.unwrap();
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(reason.contains("changed the worktree"), "{reason}");
    // The watcher already recorded the violation; the sweep must not
    // record it again.
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert_eq!(
        session_events(&run.pool, &session, EventType::WorktreeChanged)
            .await
            .len(),
        1
    );
}

#[test]
fn an_unrecorded_session_is_unverifiable_unless_the_role_is_not_read_only() {
    let dir = tempdir();
    fs::write(dir.join("p.md"), "review it").unwrap();
    let mut def = WorkflowDefinition::parse(
        READ_ONLY_WORKFLOW.replace("ROLE", "reviewer").as_str(),
        &dir,
    )
    .unwrap();
    assert!(unverified_note(&def, "review", &"boom").is_some());
    def.roles.get_mut("reviewer").unwrap().read_only = false;
    assert!(unverified_note(&def, "review", &"boom").is_none());
    def.roles.remove("reviewer");
    let note = unverified_note(&def, "review", &"boom").unwrap();
    assert!(note.contains("could not verify"), "{note}");
    // A stage that is not an agent_turn is unverifiable too, and the
    // note names the stage rather than an invented role.
    let note = unverified_note(&def, "verified", &"boom").unwrap();
    assert!(note.contains("stage 'verified'"), "{note}");
    assert!(!note.contains("'unknown'"), "{note}");
}

#[tokio::test]
async fn a_content_only_change_to_a_dirty_file_is_named_as_such() {
    let run = start_read_only_task_prepped(
        "reviewer",
        true,
        "printf a >> README.md",
        json!({}),
        ro_steps(&["printf b >> README.md"]),
        None,
        None,
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("git status or file contents changed (1 entries)"),
        "{reason}"
    );
}

#[tokio::test]
async fn the_restart_sweep_says_so_when_it_cannot_find_a_read_only_turns_session() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt"},
            {"op": "usage_limit"},
        ]),
    )
    .await;
    stuck_reason(&run.pool, &run.task_id).await;
    sqlx::query("UPDATE tasks SET status = 'open', stuck_reason = NULL WHERE id = ?")
        .bind(&run.task_id)
        .execute(&run.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE sessions RENAME TO sessions_gone")
        .execute(&run.pool)
        .await
        .unwrap();
    let binary = named_script_binary(&run.dir, "fake-claude-ro-sweep-err", ro_steps(&[]));
    let engine = engine_with_adapter_and_workflows_dir(run.pool.clone(), &binary, &run.dir);
    engine.park_interrupted_turns().await.unwrap();
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("could not verify that read-only role 'reviewer'"),
        "{reason}"
    );
    assert!(
        reason.contains("inspect and reset the worktree before retrying")
            && !reason.contains("outcome was not applied"),
        "{reason}"
    );
}

/// Runs a read-only turn that touches a file and then hangs, waits for
/// the file, and puts the session in the given end state by hand so the
/// watcher sees it on its next poll.
async fn read_only_turn_cut_off_with(
    status: SessionStatus,
    end_reason: SessionEndReason,
) -> (ReadOnlyRun, String) {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt && sleep 3"},
        ]),
    )
    .await;
    for _ in 0..400 {
        if run.worktree_dir.join("sneaky.txt").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(run.worktree_dir.join("sneaky.txt").exists());
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    sessions::update_status(&run.pool, &session.id, status, None, Some(end_reason))
        .await
        .unwrap();
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    let changed = session_events(&run.pool, &session, EventType::WorktreeChanged).await;
    assert_eq!(changed.len(), 1, "{reason}");
    (run, reason)
}

#[tokio::test]
async fn a_read_only_turn_whose_session_cannot_be_read_is_checked() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt && sleep 3"},
        ]),
    )
    .await;
    for _ in 0..400 {
        if run.worktree_dir.join("sneaky.txt").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(run.worktree_dir.join("sneaky.txt").exists());
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    // Break reads of the sessions row but leave the event insert (which
    // selects only `id` and `task_id`) working.
    sqlx::query("ALTER TABLE sessions RENAME COLUMN model TO model_gone")
        .execute(&run.pool)
        .await
        .unwrap();
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("lost track") && reason.contains("changed the worktree"),
        "{reason}"
    );
    let changed = events::list_for_session(&run.pool, &session.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::WorktreeChanged)
        .count();
    assert_eq!(changed, 1, "{reason}");
}

#[tokio::test]
async fn an_abnormal_end_that_cannot_be_verified_does_not_claim_an_outcome_was_dropped() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "printf 'gitdir: /nonexistent\\n' > .git && sleep 3"},
        ]),
    )
    .await;
    for _ in 0..400 {
        let pointer = std::fs::read_to_string(run.worktree_dir.join(".git"));
        if pointer.is_ok_and(|p| p.contains("/nonexistent")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    sessions::update_status(
        &run.pool,
        &session.id,
        SessionStatus::Idle,
        None,
        Some(SessionEndReason::Reaped),
    )
    .await
    .unwrap();
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("could not verify")
            && reason.contains("'reviewer'")
            && reason.contains("Inspect the worktree, then choco task retry"),
        "{reason}"
    );
    assert!(!reason.contains("outcome was not applied"), "{reason}");
}

#[tokio::test]
async fn a_reaped_read_only_turn_is_checked() {
    let (_run, reason) =
        read_only_turn_cut_off_with(SessionStatus::Idle, SessionEndReason::Reaped).await;
    assert!(
        reason.contains("idle reaper") && reason.contains("changed the worktree"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_daemon_stopped_read_only_turn_is_checked() {
    let (_run, reason) =
        read_only_turn_cut_off_with(SessionStatus::Exited, SessionEndReason::DaemonStopped).await;
    assert!(reason.contains("changed the worktree"), "{reason}");
}

#[tokio::test]
async fn a_lingering_read_only_turn_is_checked() {
    let run = start_read_only_task_full(
        "reviewer",
        false,
        json!({}),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": "touch sneaky.txt"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
            {"op": "emit_forever", "text": "still writing files"},
        ]),
        None,
        Some(fast_turn_timers()),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("kept running") && reason.contains("changed the worktree"),
        "{reason}"
    );
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert_eq!(
        session_events(&run.pool, &session, EventType::WorktreeChanged)
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn a_baseline_that_cannot_be_taken_stops_the_turn_before_it_starts() {
    let run = start_read_only_task("reviewer", true, json!({}), ro_steps(&[])).await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("could not record the worktree baseline")
            && reason.contains("not a git repository"),
        "{reason}"
    );
    assert!(
        runs_for_stage(&run.pool, &run.task_id, "review")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_coder_turn_is_never_checked() {
    let run = start_read_only_task(
        "coder",
        false,
        json!({}),
        ro_steps(&["git commit -q --allow-empty -m work"]),
    )
    .await;
    wait_until_stage(&run.pool, &run.task_id, "verified").await;
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert!(
        session_events(&run.pool, &session, EventType::WorktreeBaseline)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_check_that_cannot_run_fails_closed() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["printf 'gitdir: /nonexistent\\n' > .git"]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("could not verify") && reason.contains("'reviewer'"),
        "{reason}"
    );
    assert!(
        reason.contains("The turn's outcome was not applied"),
        "{reason}"
    );
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert!(
        session_events(&run.pool, &session, EventType::TurnOutcome)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_read_only_turn_that_switches_branch_parks_the_task() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["git checkout -q -b sneaky-branch"]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(
        reason.contains("branch refs/heads/") && reason.contains("→ refs/heads/sneaky-branch"),
        "{reason}"
    );
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    let changed = session_events(&run.pool, &session, EventType::WorktreeChanged).await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["changes"][0]["field"], "branch");
}

/// Deletes the baseline of a finished clean read-only turn, then runs
/// the post-turn check by hand against `stage`.
async fn recheck_without_baseline(stage: &str) -> String {
    let run = start_read_only_task("reviewer", false, json!({}), ro_steps(&[])).await;
    wait_until_stage(&run.pool, &run.task_id, "verified").await;
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    sqlx::query("DELETE FROM events WHERE session_id = ? AND event_type = 'worktree_baseline'")
        .bind(&session.id)
        .execute(&run.pool)
        .await
        .unwrap();
    run._engine
        .finish_turn(&run.task_id, &run.def, stage, None, &session.id)
        .await;
    stuck_reason(&run.pool, &run.task_id).await
}

#[tokio::test]
async fn a_check_with_no_baseline_fails_closed() {
    let reason = recheck_without_baseline("review").await;
    assert!(
        reason.contains("could not verify") && reason.contains("no worktree baseline"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_check_for_an_unknown_stage_fails_closed() {
    let reason = recheck_without_baseline("no_such_stage").await;
    assert!(
        reason.contains("could not verify") && reason.contains("no_such_stage"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_baseline_that_cannot_be_written_marks_the_session_start_failed() {
    let run = start_read_only_task_with_sql(
        "reviewer",
        false,
        json!({}),
        ro_steps(&[]),
        Some(
            "CREATE TRIGGER no_baseline BEFORE INSERT ON events \
                 WHEN NEW.event_type = 'worktree_baseline' \
                 BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
    )
    .await;
    // An entry stage's start failure is returned to the caller of
    // `start_task` (which parks the task); the session is what's pinned.
    let session = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    assert_eq!(session.status, SessionStatus::Exited);
    assert_eq!(session.end_reason, Some(SessionEndReason::StartFailed));
}

#[tokio::test]
async fn a_failed_worktree_changed_write_still_parks_the_task() {
    let run = start_read_only_task_with_sql(
        "reviewer",
        false,
        json!({}),
        ro_steps(&["touch sneaky.txt"]),
        Some(
            "CREATE TRIGGER no_changed BEFORE INSERT ON events \
                 WHEN NEW.event_type = 'worktree_changed' \
                 BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(reason.contains("git status changed"), "{reason}");
}

#[tokio::test]
async fn resuming_a_session_with_no_baseline_does_not_start_the_agent() {
    let run = start_read_only_task(
        "reviewer",
        false,
        json!({}),
        json!([{"op": "read_turn"}, {"op": "usage_limit"}]),
    )
    .await;
    let reason = stuck_reason(&run.pool, &run.task_id).await;
    assert!(reason.contains("interrupted"), "{reason}");
    let first = runs_for_stage(&run.pool, &run.task_id, "review")
        .await
        .remove(0);
    sqlx::query("DELETE FROM events WHERE session_id = ? AND event_type = 'worktree_baseline'")
        .bind(&first.id)
        .execute(&run.pool)
        .await
        .unwrap();

    let binary = named_script_binary(&run.dir, "fake-claude-ro-nobase", ro_steps(&[]));
    let engine = engine_with_adapter_and_workflows_dir(run.pool.clone(), &binary, &run.dir);
    let result = engine.retry_task(&run.task_id, RetryMode::Auto).await;
    let text = match result {
        Err(err) => err.to_string(),
        Ok(_) => stuck_reason(&run.pool, &run.task_id).await,
    };
    assert!(text.contains("no worktree baseline"), "{text}");
    assert_eq!(
        runs_for_stage(&run.pool, &run.task_id, "review")
            .await
            .len(),
        1,
        "no second session may be spawned"
    );
}

// ---- #92: retry resumes an interrupted session ----

/// Writes a `fake_claude_script.py` wrapper, named so several can exist
/// side by side in one test — a retry drives a *different* script than
/// the run it is retrying.
fn named_script_binary(dir: &Path, name: &str, steps: Value) -> String {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join(format!("{name}.json"));
    fs::write(&script, steps.to_string()).unwrap();
    let wrapper = dir.join(name);
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nFAKE_CLAUDE_SCRIPT='{}' exec '{}' \"$@\"\n",
            script.display(),
            fixture_binary("fake_claude_script.py"),
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    wrapper.display().to_string()
}

/// A one-`agent_turn` workflow on disk, so `retry_task` can re-resolve
/// it by name the way it does for a real task.
fn coding_workflow(dir: &Path) -> Arc<WorkflowDefinition> {
    let yaml = r#"
name: coding-only
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
    fs::write(dir.join("coder-turn.md"), "implement the thing").unwrap();
    fs::write(dir.join("coding-only.yaml"), yaml).unwrap();
    Arc::new(WorkflowDefinition::parse(yaml, dir).unwrap())
}

/// Runs `coding` until a usage limit interrupts it, leaving the task
/// stuck with one `interrupted` run — #88's situation, reproduced.
async fn task_stuck_on_an_interrupted_turn(
    pool: &SqlitePool,
    dir: &Path,
) -> (String, Arc<WorkflowDefinition>) {
    let def = coding_workflow(dir);
    let task_id = seed_task(pool, &def.name).await;
    let binary = named_script_binary(
        dir,
        "fake-claude-interrupted",
        json!([
            {"op": "read_turn"},
            {"op": "text", "text": "editing files"},
            {"op": "usage_limit"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(pool, &task_id, "stuck").await;
    (task_id, def)
}

async fn runs_for_stage(pool: &SqlitePool, task_id: &str, stage: &str) -> Vec<Session> {
    sessions::list_for_task(pool, task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.stage == stage)
        .collect()
}

/// The stage's run that isn't `earlier` — i.e. the one a retry just
/// opened. Picked by id rather than by position: `list_for_task` orders
/// by a random uuid, which says nothing about which run came first.
async fn run_after(pool: &SqlitePool, task_id: &str, stage: &str, earlier: &Session) -> Session {
    let runs = runs_for_stage(pool, task_id, stage).await;
    assert_eq!(runs.len(), 2, "expected exactly two runs: {runs:?}");
    runs.into_iter()
        .find(|r| r.id != earlier.id)
        .expect("the earlier run is one of the two")
}

#[tokio::test]
async fn an_interrupted_turn_marks_the_task_stuck_saying_so() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (task_id, _def) = task_stuck_on_an_interrupted_turn(&pool, &dir).await;

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("interrupted by a usage limit") && reason.contains("retry"),
        "the stuck reason should name the interruption and the way out: {reason}"
    );
    let runs = runs_for_stage(&pool, &task_id, "coding").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].end_reason, Some(SessionEndReason::Interrupted));
}

#[tokio::test]
async fn retry_resumes_the_session_a_usage_limit_interrupted() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (task_id, _def) = task_stuck_on_an_interrupted_turn(&pool, &dir).await;
    let interrupted_run = runs_for_stage(&pool, &task_id, "coding").await[0].clone();

    // The resumed turn repeats back what it was sent, then finishes.
    let resumed_binary = named_script_binary(
        &dir,
        "fake-claude-resumed",
        json!([
            {"op": "echo_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &resumed_binary, &dir);
    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();

    assert_eq!(
        outcome,
        RetryOutcome {
            stage: "coding".to_string(),
            resumed: true,
            adapter_session_id: interrupted_run.adapter_session_id.clone(),
            fresh_reason: None,
            rewatched: false,
        }
    );
    wait_until_task_status(&pool, &task_id, "closed").await;

    // A second run, pointing at the first, and continuing its session:
    // the fake takes its session id from `--resume`, so these matching
    // is proof the flag was actually passed.
    let resumed_run = run_after(&pool, &task_id, "coding", &interrupted_run).await;
    assert_eq!(
        resumed_run.resumed_from.as_deref(),
        Some(interrupted_run.id.as_str())
    );
    assert_eq!(
        resumed_run.adapter_session_id,
        interrupted_run.adapter_session_id
    );

    // And it was told it had been interrupted, rather than handed the
    // stage's prompt for a second time.
    let echoed = events::list_for_session(&pool, &resumed_run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| e.payload["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        echoed.contains("interrupted") && echoed.contains("git status"),
        "the resumed turn should be told what happened: {echoed}"
    );
    assert!(
        !echoed.contains("implement the thing"),
        "the stage prompt should not be sent again: {echoed}"
    );

    // The timeline says a resume happened, and names what it resumed.
    let trail = stage_trail(&pool, &task_id).await;
    assert!(
        trail
            .iter()
            .any(|(stage, via)| stage == "coding" && via == &json!("retry_resume")),
        "expected a retry_resume transition: {trail:?}"
    );
    let notes: Vec<String> = events::list_for_session(&pool, &resumed_run.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::SessionNote)
        .map(|e| {
            e.payload["message"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        notes.iter().any(|note| note.contains(&interrupted_run.id)),
        "expected a resume note naming the previous run: {notes:?}"
    );
}

#[tokio::test]
async fn retry_fresh_starts_a_new_session_even_when_one_could_be_resumed() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (task_id, _def) = task_stuck_on_an_interrupted_turn(&pool, &dir).await;
    let interrupted_run = runs_for_stage(&pool, &task_id, "coding").await[0].clone();

    let binary = named_script_binary(
        &dir,
        "fake-claude-fresh",
        json!([
            {"op": "echo_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    let outcome = engine.retry_task(&task_id, RetryMode::Fresh).await.unwrap();

    assert!(!outcome.resumed);
    assert_eq!(outcome.adapter_session_id, None);
    assert_eq!(
        outcome.fresh_reason.as_deref(),
        Some("a fresh start was asked for")
    );
    wait_until_task_status(&pool, &task_id, "closed").await;

    let fresh_run = run_after(&pool, &task_id, "coding", &interrupted_run).await;
    assert_eq!(fresh_run.resumed_from, None);
    assert_ne!(
        fresh_run.adapter_session_id, interrupted_run.adapter_session_id,
        "a fresh start is a new session"
    );
    // And it got the stage's own prompt back, not a resume message.
    let echoed = events::list_for_session(&pool, &fresh_run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| e.payload["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(echoed.contains("implement the thing"), "{echoed}");
}

#[tokio::test]
async fn retry_starts_fresh_when_the_turn_failed_on_its_own() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let task_id = seed_task(&pool, &def.name).await;
    // Ends its turn without ever reporting: the agent's own failure,
    // not an interruption, so there is nothing safe to resume.
    let binary = named_script_binary(
        &dir,
        "fake-claude-silent",
        json!([
            {"op": "read_turn"},
            {"op": "text", "text": "hmm"},
            {"op": "result"},
            {"op": "exit"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;
    assert_eq!(
        runs_for_stage(&pool, &task_id, "coding").await[0].end_reason,
        Some(SessionEndReason::NoReport)
    );

    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(!outcome.resumed, "a no_report run must not be resumed");
    // And the operator is told why, rather than left to infer it.
    let why = outcome.fresh_reason.unwrap();
    assert!(why.contains("no_report"), "{why}");
}

#[tokio::test]
async fn retry_resume_refuses_rather_than_quietly_starting_fresh() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let task_id = seed_task(&pool, &def.name).await;
    let binary = named_script_binary(
        &dir,
        "fake-claude-silent",
        json!([
            {"op": "read_turn"},
            {"op": "text", "text": "hmm"},
            {"op": "result"},
            {"op": "exit"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &binary, &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let err = engine
        .retry_task(&task_id, RetryMode::Resume)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotResumable(why) if why.contains("no_report")),
        "expected a NotResumable naming the end reason: {err}"
    );
    // And nothing was written: the task is still stuck, with one run.
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert_eq!(runs_for_stage(&pool, &task_id, "coding").await.len(), 1);
}

/// The resume decision itself, over the cases a live run can reach —
/// driven against the rows directly, since three interruptions in a row
/// is a lot of subprocess to spend on a rule this narrow.
#[tokio::test]
async fn only_an_outside_interruption_with_a_session_is_resumable() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let task_id = seed_task(&pool, &def.name).await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let coding = &def.stages["coding"];
    let finished = &def.stages["finished"];

    // One ended run per case, so they can't interfere with each other.
    let ended = async |session: Option<&str>, reason: Option<SessionEndReason>| -> Session {
        let run = sessions::create(
            &pool,
            sessions::NewSession {
                task_id: &task_id,
                stage: "coding",
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        if let Some(session) = session {
            sessions::set_adapter_session_id(&pool, &run.id, session)
                .await
                .unwrap();
        }
        sessions::update_status(
            &pool,
            &run.id,
            SessionStatus::Exited,
            Some(Utc::now()),
            reason,
        )
        .await
        .unwrap()
        .unwrap()
    };

    let interrupted = ended(Some("session-a"), Some(SessionEndReason::Interrupted)).await;
    assert_eq!(
        engine
            .resumable_session(&task, &def, coding, Some(&interrupted))
            .await
            .unwrap(),
        Ok(ResumeSession {
            adapter_session_id: "session-a".to_string(),
            cli_adapter: "claude".to_string(),
            previous_session_id: interrupted.id.clone(),
            end_reason: SessionEndReason::Interrupted,
        })
    );

    // The reaper closing a session is the other thing done *to* a turn.
    let reaped = ended(Some("session-b"), Some(SessionEndReason::Reaped)).await;
    assert!(
        engine
            .resumable_session(&task, &def, coding, Some(&reaped))
            .await
            .unwrap()
            .is_ok(),
        "a reaper-closed turn is resumable too"
    );

    // The agent's own failures, and a crash, are not.
    for reason in [
        Some(SessionEndReason::NoReport),
        Some(SessionEndReason::Lingered),
        Some(SessionEndReason::Cancelled),
        Some(SessionEndReason::StartFailed),
        None,
    ] {
        let run = ended(Some("session-c"), reason).await;
        assert!(
            engine
                .resumable_session(&task, &def, coding, Some(&run))
                .await
                .unwrap()
                .is_err(),
            "{reason:?} must not be resumable"
        );
    }

    // Interrupted, but with no session recorded: nothing to resume.
    let sessionless = ended(None, Some(SessionEndReason::Interrupted)).await;
    assert!(
        engine
            .resumable_session(&task, &def, coding, Some(&sessionless))
            .await
            .unwrap()
            .is_err()
    );

    // A stage with no session at all, and a stage with no run yet.
    assert!(
        engine
            .resumable_session(&task, &def, finished, Some(&interrupted))
            .await
            .unwrap()
            .is_err(),
        "a terminal stage has no session to resume"
    );
    assert!(
        engine
            .resumable_session(&task, &def, coding, None)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn a_session_resumed_too_many_times_in_a_row_has_to_start_over() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let task_id = seed_task(&pool, &def.name).await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let coding = &def.stages["coding"];

    let new_run = || sessions::NewSession {
        task_id: &task_id,
        stage: "coding",
        role: "coder",
        cli_adapter: "claude",
        model: "sonnet",
    };
    let interrupt = async |run: &Session| {
        sessions::update_status(
            &pool,
            &run.id,
            SessionStatus::Exited,
            Some(Utc::now()),
            Some(SessionEndReason::Interrupted),
        )
        .await
        .unwrap()
        .unwrap()
    };

    // The original interrupted run, then a chain of resumes of it, each
    // interrupted again.
    let mut previous = sessions::create(&pool, new_run()).await.unwrap();
    sessions::set_adapter_session_id(&pool, &previous.id, "session-a")
        .await
        .unwrap();
    previous = interrupt(&previous).await;

    // Three resumes are allowed; the fourth is where the cap bites.
    for resume in 1..=MAX_CONSECUTIVE_RESUMES + 1 {
        let decision = engine
            .resumable_session(&task, &def, coding, Some(&previous))
            .await
            .unwrap();
        if resume <= MAX_CONSECUTIVE_RESUMES {
            assert!(decision.is_ok(), "resume {resume} should still be allowed");
        } else {
            let why = decision.expect_err("the cap should have been reached");
            assert!(
                why.contains("already been resumed"),
                "the refusal should say why: {why}"
            );
            break;
        }
        let run = sessions::create_resumed(
            &pool,
            new_run(),
            sessions::ResumedFrom {
                session_id: &previous.id,
                adapter_session_id: "session-a",
            },
        )
        .await
        .unwrap();
        previous = interrupt(&run).await;
    }

    // A fresh start clears it: the next interruption is resumable again.
    let fresh = sessions::create(&pool, new_run()).await.unwrap();
    sessions::set_adapter_session_id(&pool, &fresh.id, "session-b")
        .await
        .unwrap();
    let fresh = interrupt(&fresh).await;
    assert!(
        engine
            .resumable_session(&task, &def, coding, Some(&fresh))
            .await
            .unwrap()
            .is_ok(),
        "a session that has not been resumed before is not near the cap"
    );
}

#[tokio::test]
async fn retry_task_that_fails_synchronously_returns_enter_error_and_re_marks_stuck() {
    // Review, X-4 round 2: the other "fails again" test fails
    // asynchronously inside the spawned shell runner, so it never
    // exercises `retry_task_locked`'s own `Err(err) => { mark_stuck; ...
    // }` arm — this does, by re-entering an `agent_turn` whose session
    // start fails synchronously, inside `enter_stage` itself.
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    std::fs::write(
        workflows_dir.join("broken.yaml"),
        r#"
name: broken
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
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        "/no/such/binary-3f6c9a",
        &workflows_dir,
    );

    engine
        .create_task(&project_id, "broken", "t", "hello", json!({}))
        .await
        .unwrap_err();
    let tasks = tasks::list(&pool, Some(&project_id), None).await.unwrap();
    let task_id = tasks[0].id.clone();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    // Same broken binary is still all this engine has, so re-entering
    // `chatting` fails the same way, synchronously, inside `enter_stage`
    // — before `retry_task_locked` ever returns to its caller.
    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(matches!(err, RetryTaskError::Enter(_)), "{err:?}");

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("chatting") && r.contains("retry failed")),
        "{:?}",
        task.stuck_reason
    );
}

#[tokio::test]
async fn a_template_failure_marks_the_task_stuck_with_exactly_one_error_event() {
    // Review, X-4 round 2: `enter_stage` already appends its own
    // `Error` event for a template failure before returning it; the
    // catch-all `mark_stuck` used to append a *second* one for the
    // same failure. This drives that path end to end and checks the
    // event count, not just the reason text.
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let prompt_path = dir.join("coder-turn.md");
    std::fs::write(&prompt_path, "do the thing").unwrap();
    let yaml = r#"
name: shell-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  run:
    kind: shell
    command: "true"
    on: { done: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
    // Valid at parse time — the loader validates the prompt file's
    // template syntax against *this* content. The rewrite below only
    // changes what `enter_agent_turn` reads back at runtime, once
    // `run` has already finished and advancing into `coding` tries to
    // render it.
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    std::fs::write(&prompt_path, "do the thing {{ task.input").unwrap();

    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();

    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("coding") && r.contains("template")),
        "{:?}",
        task.stuck_reason
    );

    let error_events: Vec<_> = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::Error)
        .collect();
    assert_eq!(
        error_events.len(),
        1,
        "a template failure must record exactly one Error event, not one from \
             `enter_stage` and a second from `mark_stuck`: {error_events:?}"
    );
}

/// `finish_turn`'s catch-all (#61) had no test before this task: an
/// `agent_turn` completes cleanly, but the stage its edge leads into
/// can't be entered. Both turns share one adapter binary (so a failed
/// spawn can't be what trips this), so the second stage is failed a
/// different way — a prompt template that fails to render, the same
/// technique `a_template_failure_marks_the_task_stuck_with_exactly_one_error_event`
/// uses for `finish_detached`'s catch-all.
#[tokio::test]
async fn a_turn_completing_into_a_stage_that_cannot_start_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("first-turn.md"), "do the first thing").unwrap();
    let second_prompt = dir.join("second-turn.md");
    std::fs::write(&second_prompt, "do the second thing").unwrap();
    let yaml = r#"
name: turn-then-turn
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  first:
    kind: agent_turn
    role: coder
    prompt_file: first-turn.md
    on: { done: second }
  second:
    kind: agent_turn
    role: coder
    prompt_file: second-turn.md
    on: { done: finished }
  finished:
    kind: terminal
"#;
    // Valid at parse time — `WorkflowDefinition::parse` validates the
    // prompt file's template syntax against *this* content. The
    // rewrite below only changes what `enter_agent_turn` reads back at
    // runtime, once `first` has already completed and advancing into
    // `second` tries to render it.
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    std::fs::write(&second_prompt, "do the second thing {{ task.input").unwrap();

    let engine = engine_with_adapter(pool.clone(), &reply_binary(&dir, "ok"));
    engine.start_task(&task_id, &def, None).await.unwrap();

    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "second");
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("second") && reason.contains("first"),
        "{reason:?}"
    );

    let error_events: Vec<_> = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::Error)
        .collect();
    assert_eq!(
        error_events.len(),
        1,
        "a template failure must record exactly one Error event: {error_events:?}"
    );
}

#[tokio::test]
async fn create_task_that_fails_before_reaching_a_stage_cannot_be_retried() {
    // Review, X-4 round 2: `start_task` writes `workflow_state` before
    // it ever calls `enter_stage` — a failure before that point (a
    // `worktree: true` workflow with no `config.cwd`, here) leaves no
    // stage for `retry_task` to re-enter. The reason, and every
    // `TaskStuck` hint, must say so rather than pointing at a retry
    // that can only ever 409.
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    std::fs::write(
        workflows_dir.join("worktree-entry.yaml"),
        r#"
name: worktree-entry
worktree: true
stages:
  run:
    kind: shell
    command: "true"
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .unwrap();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &workflows_dir);

    let err = engine
        .create_task(&project_id, "worktree-entry", "t", "hello", json!({}))
        .await
        .unwrap_err();

    let tasks = tasks::list(&pool, Some(&project_id), None).await.unwrap();
    assert_eq!(tasks.len(), 1);
    let task_id = tasks[0].id.clone();
    match &err {
        CreateTaskError::Start { task_id: id, .. } => assert_eq!(id, &task_id),
        other => panic!("expected CreateTaskError::Start, got {other:?}"),
    }
    assert_eq!(tasks[0].status, "stuck");
    assert!(
        tasks[0]
            .stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("cannot be retried")),
        "{:?}",
        tasks[0].stuck_reason
    );
    assert!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .is_none(),
        "start_task must have failed before workflow_state was ever created"
    );

    let retry_err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(retry_err, RetryTaskError::NoWorkflowState),
        "{retry_err:?}"
    );

    let msg_err = engine
        .send_message_or_resume(&task_id, "hello")
        .await
        .unwrap_err();
    assert!(
        matches!(
            msg_err,
            SendMessageOrResumeError::TaskStuck {
                can_retry: false,
                ..
            }
        ),
        "{msg_err:?}"
    );
    assert!(
        msg_err.to_string().contains("choco task cancel"),
        "{msg_err}"
    );
    assert!(
        !msg_err.to_string().contains("choco task retry"),
        "{msg_err}"
    );
}

#[tokio::test]
async fn retry_task_on_an_open_task_is_not_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = human_gate_chain_def();
    // An open task's workflow is loaded to see whether a watcher timed out.
    std::fs::write(
        dir.join("gated.yaml"),
        "name: gated\nstages:\n  gate:\n    kind: human_gate\n    on: { resumed: done }\n  done:\n    kind: terminal\n",
    )
    .unwrap();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();

    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotStuck(status) if status == "open"),
        "{err:?}"
    );
}

#[tokio::test]
async fn retry_task_on_a_closed_task_is_not_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker");
    std::fs::write(&marker, "").unwrap();
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;

    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotStuck(status) if status == "closed"),
        "{err:?}"
    );
}

#[tokio::test]
async fn retry_task_on_a_cancelled_task_is_not_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let def = human_gate_chain_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    engine.cancel_task(&task_id, false).await.unwrap();

    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotStuck(status) if status == "cancelled"),
        "{err:?}"
    );
}

#[tokio::test]
async fn retry_task_on_an_unknown_id_is_no_such_task() {
    let pool = connect_in_memory().await.unwrap();
    let engine = engine_with_adapter(pool.clone(), "unused");
    let err = engine
        .retry_task("does-not-exist", RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(matches!(err, RetryTaskError::NoSuchTask), "{err:?}");
}

#[tokio::test]
async fn send_message_or_resume_on_a_stuck_task_is_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let err = engine
        .send_message_or_resume(&task_id, "hello")
        .await
        .unwrap_err();
    // This task reached a real stage (`run`) before getting stuck, so
    // `retry_task` can re-enter it — the hint should say so.
    assert!(
        matches!(
            err,
            SendMessageOrResumeError::TaskStuck {
                can_retry: true,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("choco task retry"), "{err}");
}

#[tokio::test]
async fn cancelling_a_stuck_task_succeeds_and_clears_the_reason() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker"); // never created
    let def = write_marker_shell_workflow(&dir, &marker);
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    engine.cancel_task(&task_id, false).await.unwrap();

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
    assert_eq!(task.stuck_reason, None);
}

// ---- turn completion, isolation and cancel (#90) ----

fn engine_with_turn_timers(
    pool: SqlitePool,
    binary: &str,
    timers: crate::session::TurnTimers,
) -> Arc<WorkflowEngine> {
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(binary));
    let events_notify = Arc::new(Notify::new());
    let session_manager = SessionManager::with_turn_timers(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
        timers,
    );
    WorkflowEngine::new(
        pool,
        session_manager,
        PathBuf::from("."),
        None,
        events_notify,
    )
}

fn fast_turn_timers() -> crate::session::TurnTimers {
    crate::session::TurnTimers {
        grace: StdDuration::from_millis(400),
        nudge_after: StdDuration::from_millis(150),
        max_nudges: 1,
    }
}

/// A `fake_claude_script.py` wrapper following `steps`.
/// The single-script case: one fake per test directory.
fn script_binary(dir: &Path, steps: Value) -> String {
    named_script_binary(dir, "fake-claude-script", steps)
}

fn single_turn_yaml() -> &'static str {
    r#"
name: single
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
"#
}

/// #88's failure, end to end: the coder ends its turn without reporting
/// (it was waiting on a background sub-agent). The workflow must not
/// advance on that `result`; once nudging runs out, the task parks.
#[tokio::test]
async fn a_turn_that_never_reports_parks_the_task_instead_of_advancing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(single_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let binary = script_binary(
        &dir,
        json!([
            {"op": "read_turn"},
            {"op": "text", "text": "delegated it to a background agent"},
            {"op": "result"},
            {"op": "answer_every_turn", "text": "still waiting"},
        ]),
    );
    let engine = engine_with_turn_timers(pool.clone(), &binary, fast_turn_timers());

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "coding");
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("without calling report_outcome")),
        "{:?}",
        task.stuck_reason
    );
}

/// The turn reported and ended, but its process kept going. The stage
/// must not advance while it runs, and once it's killed the task parks
/// rather than moving past work that may still have been landing.
#[tokio::test]
async fn a_turn_whose_process_lingers_after_reporting_parks_the_task() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(single_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let binary = script_binary(
        &dir,
        json!([
            {"op": "read_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
            {"op": "emit_forever", "text": "still writing files"},
        ]),
    );
    let engine = engine_with_turn_timers(pool.clone(), &binary, fast_turn_timers());

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;

    let trail = stage_trail(&pool, &task_id).await;
    assert!(
        trail.iter().all(|(stage, _)| stage != "finished"),
        "must never advance past a turn whose process was still running: {trail:?}"
    );
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .as_deref()
            .is_some_and(|r| r.contains("kept running")),
        "{:?}",
        task.stuck_reason
    );
}

/// A single-shot stage with no `capture: json` may only report `done`,
/// the one outcome it advances on; a standing stage isn't told to report.
#[tokio::test]
async fn a_plain_single_shot_stage_may_report_only_done() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = Arc::new(WorkflowDefinition::parse(single_turn_yaml(), &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
    let reply = events::final_assistant_text_for_session(&pool, &run.id)
        .await
        .unwrap();
    let mcp_config = reply
        .split("|mcp_config=")
        .nth(1)
        .and_then(|rest| rest.split("|strict_mcp_config=").next())
        .expect("mcp_config field");
    let mcp_config: Value = serde_json::from_str(mcp_config).unwrap();
    assert_eq!(
        mcp_config["mcpServers"]["chocofactory"]["args"],
        json!(["mcp-serve", "--outcome", "done"])
    );
}

/// #95's seam, end to end: a stage's `report_sections:` reach the
/// spawned process's `--mcp-config` argv, so the tool that turn calls
/// actually enforces what the workflow asked for. Without this the
/// engine could pass an empty list and every other test would still
/// pass — the workflow would say the sections are required and nothing
/// would require them.
#[tokio::test]
async fn a_stages_report_sections_reach_the_spawned_process() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: reviewed
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  internal_review:
    kind: agent_turn
    role: reviewer
    capture: json
    report_sections: ["Branches → tests", "Findings"]
    on: { approved: finished, changes_requested: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let run = wait_until_run_for_stage(&pool, &task_id, "internal_review").await;
    let reply = events::final_assistant_text_for_session(&pool, &run.id)
        .await
        .unwrap();
    let mcp_config = reply
        .split("|mcp_config=")
        .nth(1)
        .and_then(|rest| rest.split("|strict_mcp_config=").next())
        .expect("mcp_config field");
    let mcp_config: Value = serde_json::from_str(mcp_config).unwrap();
    assert_eq!(
        mcp_config["mcpServers"]["chocofactory"]["args"],
        json!([
            "mcp-serve",
            "--outcome",
            "approved",
            "--outcome",
            "changes_requested",
            "--require-section",
            "Branches → tests",
            "--require-section",
            "Findings",
        ])
    );
}

/// The seam from workflow YAML to the spawned process's argv: a role's
/// `skills:` and default memory setting reach the real subprocess.
#[tokio::test]
async fn a_roles_isolation_reaches_the_spawned_process() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: isolated
roles:
  coder:
    cli: claude
    model: sonnet
    skills: [run-tests]
stages:
  coding:
    kind: agent_turn
    role: coder
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude_echo_args.py"));

    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let run = wait_until_run_for_stage(&pool, &task_id, "coding").await;
    let reply = events::final_assistant_text_for_session(&pool, &run.id)
        .await
        .unwrap();
    for expected in [
        "|setting_sources=project|",
        "|strict_mcp_config=true|",
        "|disallowed_tools=ReportFindings,ScheduleWakeup,Monitor,CronCreate,CronDelete,CronList,RemoteTrigger|",
        "|disable_auto_memory=1|",
        r#"|initialize={"skills":["run-tests"],"subtype":"initialize"}|"#,
    ] {
        assert!(reply.contains(expected), "missing {expected} in {reply}");
    }
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 delivers nothing; the call only reports whether
    // the process exists, via its return value.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// #90's cancel half: in #88 an earlier stage's run was already recorded
/// done, but its process (and the sub-agent inside it) was still alive,
/// and cancel only looked at the current stage's active run. Every live
/// process of the task has to die.
#[tokio::test]
async fn cancel_kills_every_live_session_of_the_task_not_just_the_current_one() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    // Each launch gets its own directory, so the two sessions' grandchild
    // pids land in separate files.
    let wrapper = write_script(
        &dir,
        "fake-claude-spawns-child",
        &format!(
            "#!/bin/sh\nd=$(mktemp -d '{}/run.XXXXXX')\n\
                 CHOCO_TEST_HEARTBEAT=\"$d/heartbeat\" CHOCO_TEST_CHILD_PID=\"$d/child.pid\" \
                 exec '{}' \"$@\"\n",
            dir.display(),
            fixture_binary("fake_claude_spawns_child.py"),
        ),
    );
    let engine = engine_with_adapter(pool.clone(), &wrapper.display().to_string());
    let task_id = seed_task(&pool, "single").await;
    let cfg = crate::adapter::RoleConfig {
        disallowed_tools: Vec::new(),
        cwd: std::env::temp_dir(),
        model: None,
        system_prompt: None,
        sandboxed: false,
        report_outcomes: Vec::new(),
        report_sections: Vec::new(),
        isolation: crate::adapter::Isolation::InheritOperatorConfig,
    };

    for stage in ["coding", "internal_review"] {
        let run_id = sessions::create(
            &pool,
            sessions::NewSession {
                task_id: &task_id,
                stage,
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap()
        .id;
        engine
            .session_manager
            .start(&run_id, "claude", "go", &cfg, SessionKind::Standing)
            .await
            .unwrap();
        // The earlier stage's run is recorded as finished, exactly as
        // #88's coding run was, while its process is still alive.
        if stage == "coding" {
            sessions::update_status(&pool, &run_id, SessionStatus::Idle, None, None)
                .await
                .unwrap();
        }
    }

    let pids: Vec<u32> =
        crate::test_support::wait_until("both sessions to write a child.pid", || async {
            let pids: Vec<u32> = fs::read_dir(&*dir)
                .unwrap()
                .filter_map(|entry| fs::read_to_string(entry.ok()?.path().join("child.pid")).ok())
                .filter_map(|text| text.trim().parse::<u32>().ok())
                .collect();
            if pids.len() == 2 {
                Ok(pids)
            } else {
                Err(format!("{} child pids: {pids:?}", pids.len()))
            }
        })
        .await;
    assert_eq!(pids.len(), 2, "both sessions should have started a child");
    assert!(pids.iter().all(|pid| process_alive(*pid)));

    engine.cancel_task(&task_id, false).await.unwrap();

    for pid in pids {
        crate::test_support::wait_until(&format!("pid {pid} to die after cancel"), || async {
            if !process_alive(pid) {
                Ok(())
            } else {
                Err(format!("pid {pid} still alive"))
            }
        })
        .await;
    }
}

// ---- #52: poll windows survive restarts, on a wall clock --------------

fn rfc(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// A workflow file `name.yaml` in `dir` with a `watch` poll stage.
/// `watch_kind` lets a test swap the stage for something else.
fn write_poll_flow(
    dir: &Path,
    name: &str,
    command: &str,
    timeout: Option<&str>,
    worktree: bool,
) -> Arc<WorkflowDefinition> {
    let timeout = timeout
        .map(|t| format!("    timeout: {t}\n"))
        .unwrap_or_default();
    let yaml = format!(
        r#"
name: {name}
worktree: {worktree}
stages:
  gate:
    kind: human_gate
    on: {{ resumed: watch }}
  watch:
    kind: poll
    command: "{command}"
    interval: 1s
{timeout}    capture: text
    outcomes:
      - match: "NEVER_MATCHES_XYZ"
        then: green
    on: {{ green: finished, timeout: stalled, error: stalled, again: watch }}
  finished:
    kind: terminal
  stalled:
    kind: human_gate
    on: {{ resumed: finished }}
"#
    );
    std::fs::write(dir.join(format!("{name}.yaml")), &yaml).unwrap();
    Arc::new(WorkflowDefinition::parse(&yaml, dir).unwrap())
}

async fn seed_in_project(pool: &SqlitePool, project_id: &str, name: &str, cwd: &Path) -> String {
    tasks::create(
        pool,
        tasks::NewTask {
            project_id,
            workflow_def: name,
            title: "T",
            config: json!({ "cwd": cwd.to_string_lossy() }),
            workflow_path: None,
            workflow_sha256: None,
        },
    )
    .await
    .unwrap()
    .id
}

/// A row as a dead process leaves it: at `stage`, with `payload`, no runner.
async fn seed_row(pool: &SqlitePool, task_id: &str, stage: &str, payload: Value) {
    workflow_state::create(pool, task_id, stage, "agent_turn", payload)
        .await
        .unwrap();
}

fn window_json(stage: &str, entered: DateTime<Utc>, deadline: Option<DateTime<Utc>>) -> Value {
    json!({ "stage": stage, "entered_at": rfc(entered), "deadline": deadline.map(rfc) })
}

/// True for an event a poll records for an attempt that really ran its
/// command: not the `attempt: 0` timeout entry, and not a decisive one.
fn is_real_attempt(event: &Value) -> bool {
    event["attempt"].as_u64().is_some_and(|n| n >= 1) && event.get("note").is_none()
}

async fn wait_for_poll_attempt(pool: &SqlitePool, task_id: &str) {
    crate::test_support::wait_until(
        &format!("a real poll attempt on task {task_id}"),
        || async {
            let events = poll_events(pool, task_id).await;
            if events.iter().any(is_real_attempt) {
                Ok(())
            } else {
                Err(format!("{} poll events, none a real attempt", events.len()))
            }
        },
    )
    .await
}

async fn state_of(pool: &SqlitePool, task_id: &str) -> chocofactory_core::models::WorkflowState {
    workflow_state::get(pool, task_id).await.unwrap().unwrap()
}

fn runner_slots(engine: &WorkflowEngine, task_id: &str) -> usize {
    engine
        .detached_runners
        .lock()
        .unwrap()
        .get(task_id)
        .map_or(0, |m| m.len())
}

#[test]
fn poll_window_for_handles_absent_other_stage_and_malformed() {
    assert_eq!(poll_window_for(&json!({}), "watch"), Ok(None));
    let now = Utc::now();
    let payload = json!({ "poll_window": window_json("watch", now, None) });
    assert_eq!(poll_window_for(&payload, "other"), Ok(None));
    let got = poll_window_for(&payload, "watch").unwrap().unwrap();
    assert_eq!(got.stage, "watch");
    assert_eq!(got.deadline, None);
    let bad = json!({ "poll_window": { "stage": "watch", "entered_at": "nonsense" } });
    assert!(poll_window_for(&bad, "watch").is_err());
    assert!(poll_window_for(&json!({ "poll_window": 5 }), "watch").is_err());
}

#[test]
fn remaining_budget_is_zero_once_the_deadline_passed() {
    let now = Utc::now();
    assert_eq!(
        remaining_budget(now - chrono::Duration::seconds(5), now),
        Duration::ZERO
    );
    assert_eq!(
        remaining_budget(now + chrono::Duration::seconds(5), now),
        Duration::from_secs(5)
    );
}

#[test]
fn set_poll_window_stamps_polls_and_removes_the_key_otherwise() {
    let dir = tempdir();
    let def = write_poll_flow(&dir, "unit-flow", "true", Some("6h"), false);
    let now = Utc::now();
    let mut payload = json!({ "stages": { "watch": "kept" } });
    set_poll_window(&mut payload, &def, "watch", now).unwrap();
    let window = poll_window_for(&payload, "watch").unwrap().unwrap();
    assert_eq!(window.entered_at, now);
    assert_eq!(window.deadline, Some(now + chrono::Duration::hours(6)));
    assert_eq!(payload["stages"]["watch"], json!("kept"));
    set_poll_window(&mut payload, &def, "finished", now).unwrap();
    assert!(payload.get("poll_window").is_none());
    set_poll_window(&mut payload, &def, "watch", now).unwrap();
    set_poll_window(&mut payload, &def, "no-such-stage", now).unwrap();
    assert!(payload.get("poll_window").is_none());
    let mut not_object = json!("x");
    set_poll_window(&mut not_object, &def, "watch", now).unwrap();
    assert!(not_object.get("poll_window").is_some());
}

#[tokio::test]
async fn entering_a_poll_stamps_its_window_via_start_and_advance() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "stamp-flow", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    // Advance path: gate -> watch.
    engine.start_task(&task_id, &def, None).await.unwrap();
    assert!(
        state_of(&pool, &task_id)
            .await
            .payload
            .get("poll_window")
            .is_none()
    );
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "watch");
    let window = poll_window_for(&state.payload, "watch").unwrap().unwrap();
    assert_eq!(
        window.deadline,
        Some(window.entered_at + chrono::Duration::hours(6))
    );

    // No timeout: deadline null.
    let dir2 = tempdir();
    let yaml = r#"
name: entry-poll
stages:
  watch:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    outcomes:
      - match: "NEVER_MATCHES_XYZ"
        then: green
    on: { green: finished }
  finished:
    kind: terminal
"#;
    let def2 = Arc::new(WorkflowDefinition::parse(yaml, &dir2).unwrap());
    let pool2 = connect_in_memory().await.unwrap();
    let task2 = seed_task_in(&pool2, &def2.name, &dir2).await;
    let engine2 = engine_with_adapter(pool2.clone(), "unused");
    engine2.start_task(&task2, &def2, None).await.unwrap();
    let state2 = state_of(&pool2, &task2).await;
    assert_eq!(state2.payload["poll_window"]["deadline"], Value::Null);
    assert!(state2.payload["poll_window"]["entered_at"].is_string());
    assert_eq!(
        state2.payload["arrival"],
        json!({ "from": "", "outcome": "" })
    );
    engine.abort_detached_runners(&task_id).await;
    engine2.abort_detached_runners(&task2).await;
}

#[tokio::test]
async fn leaving_a_poll_removes_the_window_and_keeps_the_capture() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: leave-flow
stages:
  watch:
    kind: poll
    command: "echo SUCCESS"
    interval: 1s
    timeout: 1h
    capture: text
    outcomes:
      - match: "SUCCESS"
        then: green
    on: { green: finished, timeout: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;
    let state = state_of(&pool, &task_id).await;
    assert!(state.payload.get("poll_window").is_none());
    assert_eq!(state.payload["stages"]["watch"], json!("SUCCESS"));
}

#[tokio::test]
async fn a_timed_out_reentry_keeps_the_previous_laps_string_capture() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "lap-flow", "echo PENDING", Some("1h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    seed_row(
            &pool,
            &task_id,
            "watch",
            json!({
                "stages": { "watch": "previous lap" },
                "poll_window": window_json("watch", now - chrono::Duration::hours(2), Some(now - chrono::Duration::hours(1))),
            }),
        )
        .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.resume_interrupted_polls().await.unwrap();
    wait_until_stage(&pool, &task_id, "stalled").await;
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.payload["stages"]["watch"], json!("previous lap"));
    assert!(state.payload.get("poll_window").is_none());
}

#[tokio::test]
async fn reentering_the_same_poll_stage_stamps_a_later_entered_at() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "again-flow", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    let first = poll_window_for(&state_of(&pool, &task_id).await.payload, "watch")
        .unwrap()
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(20)).await;
    engine.advance(&task_id, &def, "again").await.unwrap();
    let second = poll_window_for(&state_of(&pool, &task_id).await.payload, "watch")
        .unwrap()
        .unwrap();
    assert!(second.entered_at > first.entered_at);
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn a_restart_mid_poll_resumes_with_the_remaining_budget() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "resume-flow", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    let window = window_json(
        "watch",
        now - chrono::Duration::hours(1),
        Some(now + chrono::Duration::seconds(2)),
    );
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": window.clone() }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(
        report,
        PollSweepReport {
            resumed: 1,
            already_running: 0,
            stuck: 0,
            stage_kind_unrecorded: 0,
        }
    );
    assert_eq!(
        state_of(&pool, &task_id).await.payload["poll_window"],
        window
    );
    assert!(
        stage_trail(&pool, &task_id)
            .await
            .iter()
            .any(|(s, o)| s == "watch" && o == &json!("restart"))
    );
    wait_until_stage(&pool, &task_id, "stalled").await;
    assert!(
        poll_events(&pool, &task_id)
            .await
            .iter()
            .any(is_real_attempt),
        "the remaining budget must allow at least one real attempt"
    );
}

#[tokio::test]
async fn an_expired_deadline_times_out_without_running_the_command() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("ran");
    let def = write_poll_flow(
        &dir,
        "expired-flow",
        &format!("touch {}", marker.display()),
        Some("6h"),
        false,
    );
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    seed_row(&pool, &task_id, "watch", json!({ "poll_window": window_json("watch", now - chrono::Duration::hours(1), Some(now - chrono::Duration::minutes(1))) })).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.resume_interrupted_polls().await.unwrap();
    wait_until_stage(&pool, &task_id, "stalled").await;
    let event = wait_until_decisive_poll_event(&pool, &task_id).await;
    assert_eq!(event["attempt"], json!(0));
    assert_eq!(
        event["note"],
        json!(
            "timeout elapsed before the first attempt: the deadline passed while the daemon was down"
        )
    );
    assert!(
        !marker.exists(),
        "the command must not run after the deadline"
    );
}

#[tokio::test]
async fn a_sweep_does_not_double_spawn_a_live_poll() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "live-flow", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(
        report,
        PollSweepReport {
            resumed: 0,
            already_running: 1,
            stuck: 0,
            stage_kind_unrecorded: 0,
        }
    );
    assert_eq!(runner_slots(&engine, &task_id), 1);
    assert!(
        !stage_trail(&pool, &task_id)
            .await
            .iter()
            .any(|(_, o)| o == &json!("restart"))
    );
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn unrecoverable_polls_are_marked_stuck_and_healthy_ones_still_resume() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let now = Utc::now();
    let payload =
        json!({ "poll_window": window_json("watch", now, Some(now + chrono::Duration::hours(1))) });

    // (a) workflow file deleted.
    write_poll_flow(&dir, "gone-flow", "echo PENDING", Some("1h"), false);
    let a = seed_in_project(&pool, &project_id, "gone-flow", &dir).await;
    seed_row(&pool, &a, "watch", payload.clone()).await;
    std::fs::remove_file(dir.join("gone-flow.yaml")).unwrap();

    // (b) stage redefined as a shell.
    std::fs::write(
            dir.join("shell-flow.yaml"),
            "name: shell-flow\nstages:\n  watch:\n    kind: shell\n    command: \"true\"\n    on: { done: finished }\n  finished:\n    kind: terminal\n",
        )
        .unwrap();
    let b = seed_in_project(&pool, &project_id, "shell-flow", &dir).await;
    seed_row(&pool, &b, "watch", payload.clone()).await;

    // (c) enter_stage fails: worktree workflow, task has no snapshot.
    write_poll_flow(&dir, "tree-flow", "echo PENDING", Some("1h"), true);
    let c = seed_in_project(&pool, &project_id, "tree-flow", &dir).await;
    seed_row(&pool, &c, "watch", payload.clone()).await;

    // healthy
    write_poll_flow(&dir, "ok-flow", "echo PENDING", Some("1h"), false);
    let ok = seed_in_project(&pool, &project_id, "ok-flow", &dir).await;
    seed_row(&pool, &ok, "watch", payload.clone()).await;

    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(
        report,
        PollSweepReport {
            resumed: 1,
            already_running: 0,
            stuck: 3,
            stage_kind_unrecorded: 0,
        }
    );
    for id in [&a, &b, &c] {
        let task = tasks::get(&pool, id).await.unwrap().unwrap();
        assert_eq!(task.status, "stuck");
        assert!(task.stuck_reason.unwrap().contains("'watch'"));
        let errors = events::list_for_task(&pool, id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::Error)
            .count();
        assert!(errors >= 1, "expected a stuck error event");
    }
    assert_eq!(
        tasks::get(&pool, &ok).await.unwrap().unwrap().status,
        "open"
    );
    engine.abort_detached_runners(&ok).await;
}

#[tokio::test]
async fn a_malformed_window_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "bad-window", "echo PENDING", Some("1h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": { "stage": "watch" } }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.stuck, 1);
    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "stuck"
    );
}

#[tokio::test]
async fn the_sweep_leaves_non_open_and_non_poll_tasks_alone() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    write_poll_flow(&dir, "idle-flow", "echo PENDING", Some("1h"), false);
    let now = Utc::now();
    let payload =
        json!({ "poll_window": window_json("watch", now, Some(now + chrono::Duration::hours(1))) });

    let gate = seed_in_project(&pool, &project_id, "idle-flow", &dir).await;
    seed_row(&pool, &gate, "gate", payload.clone()).await;
    let mut others = vec![];
    for status in ["cancelled", "stuck", "closed"] {
        let id = seed_in_project(&pool, &project_id, "idle-flow", &dir).await;
        seed_row(&pool, &id, "watch", payload.clone()).await;
        if status == "stuck" {
            tasks::mark_stuck(&pool, &id, "because").await.unwrap();
        } else {
            tasks::update_status(&pool, &id, status).await.unwrap();
        }
        others.push((id, status));
    }
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report, PollSweepReport::default());
    assert_eq!(
        tasks::get(&pool, &gate).await.unwrap().unwrap().status,
        "open"
    );
    for (id, status) in others {
        assert_eq!(
            tasks::get(&pool, &id).await.unwrap().unwrap().status,
            status
        );
    }
    let all = events::list_for_task(&pool, &gate).await.unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn a_legacy_row_without_a_window_derives_its_deadline_in_memory_only() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "legacy-flow", "echo PENDING", Some("1h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "task": { "title": "T" } }),
    )
    .await;
    let backdated = Utc::now() - chrono::Duration::seconds(3598);
    sqlx::query("UPDATE workflow_state SET updated_at = ? WHERE task_id = ?")
        .bind(backdated)
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    let before = state_of(&pool, &task_id).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.resumed, 1);
    let after = state_of(&pool, &task_id).await;
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.payload, before.payload);
    wait_until_stage(&pool, &task_id, "stalled").await;
}

#[tokio::test]
async fn the_poll_budget_follows_the_wall_clock_not_the_monotonic_one() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "sleep-flow", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let offset = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let clock_offset = Arc::clone(&offset);
    let events_notify = Arc::new(Notify::new());
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary("unused"));
    let session_manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new_with_clock(
        pool.clone(),
        session_manager,
        dir.to_path_buf(),
        None,
        events_notify,
        Arc::new(move || {
            Utc::now()
                + chrono::Duration::seconds(clock_offset.load(std::sync::atomic::Ordering::SeqCst))
        }),
    );
    engine.start_task(&task_id, &def, None).await.unwrap();
    engine.advance(&task_id, &def, "resumed").await.unwrap();
    wait_for_poll_attempt(&pool, &task_id).await;
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "watch");
    offset.store(6 * 3600 + 1, std::sync::atomic::Ordering::SeqCst);
    wait_until_stage(&pool, &task_id, "stalled").await;
}

#[tokio::test]
async fn retrying_a_stuck_poll_stamps_a_fresh_window() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "retry-poll", "echo PENDING", Some("1h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    let mut payload = json!({
        "arrival": { "from": "gate", "outcome": "resumed" },
        "poll_window": window_json("watch", now - chrono::Duration::hours(3), Some(now - chrono::Duration::hours(2))),
    });
    seed_row(&pool, &task_id, "watch", payload.take()).await;
    let counters = json!({ "watch": { "count": 2 } });
    sqlx::query("UPDATE workflow_state SET loop_counters = ? WHERE task_id = ?")
        .bind(sqlx::types::Json(counters.clone()))
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    tasks::mark_stuck(&pool, &task_id, "x").await.unwrap();
    let entered_before = state_of(&pool, &task_id).await.stage_entered_at;
    assert!(entered_before.is_some());
    tokio::time::sleep(Duration::from_millis(5)).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_for_poll_attempt(&pool, &task_id).await;
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "watch");
    // #164: this retry rewrites the payload (fresh window) through
    // `workflow_state::update`, which must not re-stamp the entry time.
    assert_eq!(state.stage_entered_at, entered_before);
    assert!(state.updated_at > state.stage_entered_at.unwrap());
    assert_eq!(state.loop_counters, counters);
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "gate", "outcome": "resumed" })
    );
    let window = poll_window_for(&state.payload, "watch").unwrap().unwrap();
    assert!(window.deadline.unwrap() > Utc::now() + chrono::Duration::minutes(50));
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn entering_a_poll_without_a_window_is_an_invariant_error() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "nowindow", "echo PENDING", Some("1h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let err = engine
        .enter_stage(&task_id, &def, "watch", None, None, &json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::PollWindow { .. }));
    let err = engine
        .enter_stage(
            &task_id,
            &def,
            "watch",
            None,
            None,
            &json!({ "poll_window": 1 }),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::PollWindow { .. }));
}

#[tokio::test]
async fn an_overflowing_timeout_is_a_poll_window_error_and_writes_nothing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(
        &dir,
        "huge-flow",
        "echo PENDING",
        Some("3000000000h"),
        false,
    );
    let now = Utc::now();
    let mut payload = json!({});
    let err = set_poll_window(&mut payload, &def, "watch", now).unwrap_err();
    assert!(matches!(err, EngineError::PollWindow { .. }));
    assert!(payload.get("poll_window").is_none());

    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    // gate -> watch overflows: the advance must fail before any write.
    let _ = engine.advance(&task_id, &def, "resumed").await;
    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "gate");
    assert!(state.payload.get("poll_window").is_none());
    assert_eq!(runner_slots(&engine, &task_id), 0);
}

#[tokio::test]
async fn an_overflowing_timeout_fails_start_task_without_creating_state() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: huge-entry
stages:
  watch:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: 3000000000h
    outcomes:
      - match: "NEVER_MATCHES_XYZ"
        then: green
    on: { green: finished, timeout: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    let err = engine.start_task(&task_id, &def, None).await.unwrap_err();
    assert!(matches!(err, EngineError::PollWindow { .. }), "{err:?}");
    assert!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_sweep_marks_a_task_stuck_when_its_workflow_state_cannot_be_read() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let now = Utc::now();
    let payload =
        json!({ "poll_window": window_json("watch", now, Some(now + chrono::Duration::hours(1))) });
    write_poll_flow(&dir, "bad-json", "echo PENDING", Some("1h"), false);
    let bad = seed_in_project(&pool, &project_id, "bad-json", &dir).await;
    seed_row(&pool, &bad, "watch", payload.clone()).await;
    sqlx::query("UPDATE workflow_state SET payload = '{' WHERE task_id = ?")
        .bind(&bad)
        .execute(&pool)
        .await
        .unwrap();
    let ok = seed_in_project(&pool, &project_id, "bad-json", &dir).await;
    seed_row(&pool, &ok, "watch", payload).await;

    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.stuck, 1);
    assert_eq!(report.resumed, 1);
    let task = tasks::get(&pool, &bad).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert!(task.stuck_reason.unwrap().contains("workflow state"));
    assert_eq!(
        tasks::get(&pool, &ok).await.unwrap().unwrap().status,
        "open"
    );
    engine.abort_detached_runners(&ok).await;
}

#[tokio::test]
async fn the_sweep_skips_open_tasks_without_state_or_with_an_unloadable_non_poll_workflow() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    write_poll_flow(&dir, "skip-flow", "echo PENDING", Some("1h"), false);

    // Open task with no workflow_state row.
    let no_state = seed_in_project(&pool, &project_id, "skip-flow", &dir).await;

    // Non-poll stage, no window, workflow file gone: warn and skip.
    let gone = seed_in_project(&pool, &project_id, "vanished-flow", &dir).await;
    seed_row(&pool, &gone, "gate", json!({})).await;

    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report, PollSweepReport::default());
    for id in [&no_state, &gone] {
        assert_eq!(tasks::get(&pool, id).await.unwrap().unwrap().status, "open");
        assert!(events::list_for_task(&pool, id).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn retrying_a_stuck_non_poll_stage_drops_a_stale_window() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(
            dir.join("redefined.yaml"),
            "name: redefined\nstages:\n  watch:\n    kind: shell\n    command: \"sleep 30\"\n    on: { done: finished }\n  finished:\n    kind: terminal\n",
        )
        .unwrap();
    let task_id = seed_task_in(&pool, "redefined", &dir).await;
    let now = Utc::now();
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": window_json("watch", now, None) }),
    )
    .await;
    tasks::mark_stuck(&pool, &task_id, "x").await.unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    let state = state_of(&pool, &task_id).await;
    assert!(state.payload.get("poll_window").is_none());
    engine.abort_detached_runners(&task_id).await;
}

// ---- #101: `env:` and the engine's CHOCO_* variables ----

fn hostile_text(marker_dir: &Path) -> String {
    let d = marker_dir.display();
    format!("he said \"hi\" 'there' $(touch {d}/a) `touch {d}/b` ; touch {d}/c \\ %s\nsecond line")
}

/// A reviewer-style `capture: json` turn whose report carries `summary`
/// feeds `env:` into a later shell stage, which prints it back.
#[tokio::test]
async fn env_carries_agent_text_to_a_shell_stage_without_a_shell_parsing_it() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: env-flow
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { done: echo }
  echo:
    kind: shell
    command: "printf '%s' \"$X\""
    env:
      X: "{{ stages.review.summary }}"
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    let hostile = hostile_text(&dir);
    let reply = json!({"outcome": "done", "summary": hostile}).to_string();
    finish_review_turn_from_reply(&pool, &engine, &def, &task_id, &reply).await;
    wait_until_stage(&pool, &task_id, "finished").await;

    let payload = payload_of(&pool, &task_id).await;
    assert_eq!(payload["stages"]["echo"], json!(hostile));
    for marker in ["a", "b", "c"] {
        assert!(!dir.join(marker).exists(), "{marker} was created");
    }
}

#[tokio::test]
async fn the_engine_sets_the_choco_variables() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: choco-env
stages:
  show:
    kind: shell
    command: "printf '%s|%s|%s|%s' \"$CHOCO_TASK_ID\" \"$CHOCO_WORKFLOW\" \"$CHOCO_STAGE\" \"$CHOCO_ROLE_MODELS\""
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let engine = engine_with_adapter(pool.clone(), "unused");

    for (sessions_to_seed, expected) in [
        (
            vec![
                ("reviewer", "claude-opus-5-5"),
                ("coder", "claude-sonnet-5-5"),
                ("coder", "claude-sonnet-5-5"),
            ],
            "coder=claude-sonnet-5-5, reviewer=claude-opus-5-5",
        ),
        (vec![("coder", "")], "coder=default"),
        (vec![], ""),
    ] {
        let task_id = seed_task(&pool, &def.name).await;
        for (role, model) in sessions_to_seed {
            sessions::create(
                &pool,
                sessions::NewSession {
                    task_id: &task_id,
                    stage: "coding",
                    role,
                    cli_adapter: "claude",
                    model,
                },
            )
            .await
            .unwrap();
        }
        engine.start_task(&task_id, &def, None).await.unwrap();
        let event = wait_until_shell_event_for(&pool, &task_id, "show").await;
        assert_eq!(
            event["stdout_tail"],
            json!(format!("{task_id}|choco-env|show|{expected}")),
            "for sessions of {expected:?}"
        );
    }
}

#[tokio::test]
async fn unresolved_env_placeholders_join_the_commands_in_one_note() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: env-unresolved
stages:
  open_pr:
    kind: shell
    command: "printf '{\"number\": 42}'"
    capture: json
    on: { done: report }
  report:
    kind: shell
    command: "printf '%s' '{{ stages.open_pr.gone }}'"
    env:
      X: "{{ stages.open_pr.missing }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let notes: Vec<_> = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::TemplateUnresolved)
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].payload["stage"], json!("report"));
    assert_eq!(
        notes[0].payload["placeholders"],
        json!(["{{ stages.open_pr.gone }}", "{{ stages.open_pr.missing }}"])
    );
}

#[test]
fn render_env_truncates_at_a_char_boundary_and_says_so() {
    let mut env = IndexMap::new();
    env.insert("BIG".to_string(), "{{ task.input }}".to_string());
    env.insert("SMALL".to_string(), "ok".to_string());
    let big = "é".repeat(40_000); // 80,000 bytes
    let rendered = render_env(&env, &json!({"task": {"input": big}}), "s").unwrap();
    assert_eq!(rendered.truncated, vec!["BIG".to_string()]);
    let value = &rendered.pairs[0].1;
    assert!(value.len() <= MAX_ENV_VALUE_BYTES);
    let (kept_text, suffix) = value.split_once("\n[truncated by chocofactory: ").unwrap();
    assert_eq!(
        suffix,
        format!("{} of 80000 bytes]", kept_text.len()),
        "the suffix reports the kept length"
    );
    assert!(kept_text.chars().all(|c| c == 'é'));
    assert_eq!(rendered.pairs[1], ("SMALL".to_string(), "ok".to_string()));
}

#[tokio::test]
async fn an_oversized_env_value_arrives_truncated_and_is_noted() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: env-big
roles:
  reviewer:
    cli: claude
    model: sonnet
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { done: echo }
  echo:
    kind: shell
    command: "printf '%s' \"$X\""
    env:
      X: "{{ stages.review.summary }}"
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    let reply = json!({"outcome": "done", "summary": "é".repeat(40_000)}).to_string();
    finish_review_turn_from_reply(&pool, &engine, &def, &task_id, &reply).await;
    wait_until_stage(&pool, &task_id, "finished").await;

    let payload = payload_of(&pool, &task_id).await;
    let echoed = payload["stages"]["echo"].as_str().unwrap();
    assert!(echoed.len() <= MAX_ENV_VALUE_BYTES);
    assert!(
        echoed.contains("\n[truncated by chocofactory: "),
        "tail: {}",
        &echoed[echoed.len() - 80..]
    );
    assert!(echoed.ends_with(" of 80000 bytes]"));

    let note = events::list_for_task(&pool, &task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::EnvTruncated)
        .expect("a truncation note on the timeline");
    assert_eq!(note.payload["stage"], json!("echo"));
    assert_eq!(note.payload["env_truncated"], json!(["X"]));
}

#[tokio::test]
async fn a_poll_stages_env_reaches_every_attempt() {
    use std::os::unix::fs::PermissionsExt;

    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let script = dir.join("check.sh");
    std::fs::write(
            &script,
            "#!/bin/sh\nn=$(cat count 2>/dev/null || echo 0)\nn=$((n+1))\n\
             echo $n > count\nif [ $n -ge 2 ]; then echo \"SUCCESS $X\"; else echo \"PENDING $X\"; fi\n",
        )
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let yaml = format!(
        r#"
name: poll-env
stages:
  watch:
    kind: poll
    script_file: check.sh
    interval: 1s
    timeout: 30s
    env:
      X: "{{{{ task.title }}}}"
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

    let events = poll_events(&pool, &task_id).await;
    let tails: Vec<_> = events.iter().map(|e| e["stdout_tail"].clone()).collect();
    assert_eq!(tails, vec![json!("PENDING T"), json!("SUCCESS T")]);
}

// ---- #84: the startup park sweep, restart_effect and in_flight ----

const CHAT_FLOW: &str = r#"
name: chat-flow
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

const MIXED_FLOW: &str = r#"
name: mixed-flow
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  turn:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  sh:
    kind: shell
    command: "true"
    on: { done: finished }
  finished:
    kind: terminal
"#;

async fn open_task_at(
    pool: &SqlitePool,
    project_id: &str,
    workflow: &str,
    dir: &Path,
    stage: &str,
) -> String {
    let id = seed_in_project(pool, project_id, workflow, dir).await;
    seed_row(pool, &id, stage, json!({})).await;
    id
}

/// A session as a crashed daemon leaves it: `idle`, with a session to
/// resume and no end reason.
async fn crashed_session(pool: &SqlitePool, task_id: &str, stage: &str) -> Session {
    let run = sessions::create(
        pool,
        sessions::NewSession {
            task_id,
            stage,
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    sessions::set_adapter_session_id(pool, &run.id, "adapter-sess-1")
        .await
        .unwrap();
    sessions::update_status(pool, &run.id, SessionStatus::Idle, None, None)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn restart_effect_classifies_every_stage_kind() {
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "x").unwrap();
    let polls = write_poll_flow(&dir, "poll-flow", "echo PENDING", Some("1h"), false);
    let mixed = WorkflowDefinition::parse(MIXED_FLOW, &dir).unwrap();
    let chat = WorkflowDefinition::parse(CHAT_FLOW, &dir).unwrap();
    assert_eq!(
        restart_effect(&mixed.stages["turn"]),
        RestartEffect::StrandsAgentTurn
    );
    assert_eq!(
        restart_effect(&mixed.stages["sh"]),
        RestartEffect::StrandsShell
    );
    assert_eq!(
        restart_effect(&mixed.stages["finished"]),
        RestartEffect::Survives
    );
    assert_eq!(
        restart_effect(&chat.stages["chatting"]),
        RestartEffect::Survives
    );
    assert_eq!(
        restart_effect(&polls.stages["watch"]),
        RestartEffect::Survives
    );
    assert_eq!(
        restart_effect(&polls.stages["gate"]),
        RestartEffect::Survives
    );
}

#[tokio::test]
async fn parking_an_interrupted_agent_turn_records_the_session_and_retry_resumes_it() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = open_task_at(&pool, &project_id, &def.name, &dir, "coding").await;
    let crashed = crashed_session(&pool, &task_id, "coding").await;

    let resumed_binary = named_script_binary(
        &dir,
        "fake-claude-resumed",
        json!([
            {"op": "echo_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &resumed_binary, &dir);
    let report = engine.park_interrupted_turns().await.unwrap();
    assert_eq!(
        report,
        ParkReport {
            agent_turns: 1,
            shells: 0,
            stuck_other: 0
        }
    );

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert_eq!(
        task.stuck_reason.as_deref(),
        Some(agent_reason("coding").as_str())
    );
    assert_eq!(
        task.stuck_reason.unwrap(),
        "stage 'coding' was running an agent turn when the daemon stopped; 'choco task \
             retry' continues it, resuming the agent's session when it can"
    );
    let stuck_events = events::list_for_task(&pool, &task_id).await.unwrap();
    assert!(
        stuck_events
            .iter()
            .any(|e| e.event_type == EventType::Error && e.payload["stuck"] == json!(true)),
        "expected a stuck event: {stuck_events:?}"
    );
    let parked = sessions::get(&pool, &crashed.id).await.unwrap().unwrap();
    assert_eq!(parked.status, SessionStatus::Exited);
    assert_eq!(parked.end_reason, Some(SessionEndReason::DaemonStopped));

    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(outcome.resumed, "{outcome:?}");
    assert_eq!(
        outcome.adapter_session_id.as_deref(),
        Some("adapter-sess-1")
    );
    wait_until_task_status(&pool, &task_id, "closed").await;
    let resumed_run = run_after(&pool, &task_id, "coding", &crashed).await;
    assert_eq!(
        resumed_run.resumed_from.as_deref(),
        Some(crashed.id.as_str())
    );
    assert_eq!(
        resumed_run.adapter_session_id.as_deref(),
        Some("adapter-sess-1")
    );
    let echoed = events::list_for_session(&pool, &resumed_run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| e.payload["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        echoed.contains("the daemon was stopped or restarted while you were working"),
        "{echoed}"
    );
}

/// When the session cannot be recorded, the reason says so and retry
/// really does start the stage fresh.
#[tokio::test]
async fn parking_says_so_when_the_interrupted_session_cannot_be_recorded() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = open_task_at(&pool, &project_id, &def.name, &dir, "coding").await;
    let crashed = crashed_session(&pool, &task_id, "coding").await;
    sqlx::query(
        "CREATE TRIGGER block_daemon_stopped BEFORE UPDATE ON sessions \
             WHEN NEW.end_reason = 'daemon_stopped' \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let fresh_binary = named_script_binary(
        &dir,
        "fake-claude-fresh",
        json!([
            {"op": "read_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &fresh_binary, &dir);
    let report = engine.park_interrupted_turns().await.unwrap();
    assert_eq!(report.agent_turns, 1);

    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    let reason = task.stuck_reason.unwrap();
    assert!(reason.starts_with(&agent_reason("coding")), "{reason}");
    assert!(
        reason.contains("the interrupted session could not be recorded")
            && reason.contains("injected")
            && reason.ends_with("so retry will start the stage fresh"),
        "{reason}"
    );
    let session = sessions::get(&pool, &crashed.id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.end_reason, None);

    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(!outcome.resumed, "{outcome:?}");
}

/// Shutdown must kill a running shell stage's whole process group
/// before the daemon lock is released.
#[tokio::test]
async fn abort_all_detached_runners_kills_running_shell_groups() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let gc = dir.join("gc");
    let yaml = format!(
        r#"
name: hang-flow
stages:
  run:
    kind: shell
    command: "sleep 600 & echo $! > {}; wait"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
        gc.display()
    );
    std::fs::write(dir.join("hang-flow.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = open_task_at(&pool, &project_id, &def.name, &dir, "run").await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    // The row is already at `run`, as after a crash; parking then
    // retrying starts the shell runner.
    engine.park_interrupted_turns().await.unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();

    let pid: u32 = crate::test_support::wait_until("the grandchild pid file", || async {
        std::fs::read_to_string(&gc)
            .map_err(|e| e.to_string())
            .and_then(|s| s.trim().parse().map_err(|e| format!("{e}")))
    })
    .await;
    assert!(process_alive(pid));

    engine.abort_all_detached_runners().await;
    assert!(engine.detached_runners.lock().unwrap().is_empty());
    crate::test_support::wait_until("the grandchild to die", || async {
        if process_alive(pid) {
            Err("still alive".to_string())
        } else {
            Ok(())
        }
    })
    .await;
}

/// Once shutdown has begun no new runner may start: the command never
/// runs, and the task stays where it was for the next start's sweeps.
#[tokio::test]
async fn runner_spawn_after_shutdown_is_refused() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let gc = dir.join("gc");
    let yaml = format!(
        r#"
name: hang-flow
stages:
  run:
    kind: shell
    command: "sleep 600 & echo $! > {}; wait"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#,
        gc.display()
    );
    std::fs::write(dir.join("hang-flow.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = open_task_at(&pool, &project_id, &def.name, &dir, "run").await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.abort_all_detached_runners().await;

    // The helper itself refuses, and never invokes the future factory.
    let called = std::sync::atomic::AtomicBool::new(false);
    assert!(!engine.spawn_registered_runner("t", |_| {
        called.store(true, Ordering::SeqCst);
        async {}
    }));
    assert!(!called.load(Ordering::SeqCst));

    // And end to end: retry enters the shell stage, nothing runs.
    engine.park_interrupted_turns().await.unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(!gc.exists(), "the shell command must never have started");
    assert!(engine.detached_runners.lock().unwrap().is_empty());
}

#[tokio::test]
async fn parking_a_shell_stage_lets_retry_rerun_it() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let marker = dir.join("marker");
    let def = write_marker_shell_workflow(&dir, &marker);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = open_task_at(&pool, &project_id, &def.name, &dir, "run").await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);

    let report = engine.park_interrupted_turns().await.unwrap();
    assert_eq!(
        report,
        ParkReport {
            agent_turns: 0,
            shells: 1,
            stuck_other: 0
        }
    );
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert_eq!(
        task.stuck_reason.unwrap(),
        "stage 'run' was running a shell command when the daemon stopped; 'choco task \
             retry' runs it again from the start"
    );

    std::fs::write(&marker, "").unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;
}

#[tokio::test]
async fn the_park_sweep_leaves_everything_else_alone() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    write_poll_flow(&dir, "poll-flow", "echo PENDING", Some("1h"), false);
    std::fs::write(dir.join("chat-flow.yaml"), CHAT_FLOW).unwrap();
    let def = coding_workflow(&dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;

    let mut untouched = vec![];
    for (flow, stage) in [
        ("poll-flow", "watch"),
        ("poll-flow", "gate"),
        ("chat-flow", "chatting"),
    ] {
        let id = open_task_at(&pool, &project_id, flow, &dir, stage).await;
        untouched.push((id, "open"));
    }
    for status in ["stuck", "closed", "cancelled"] {
        let id = open_task_at(&pool, &project_id, &def.name, &dir, "coding").await;
        if status == "stuck" {
            tasks::mark_stuck(&pool, &id, "because").await.unwrap();
        } else {
            tasks::update_status(&pool, &id, status).await.unwrap();
        }
        untouched.push((id, status));
    }
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.park_interrupted_turns().await.unwrap();
    assert_eq!(report, ParkReport::default());
    for (id, status) in untouched {
        assert_eq!(
            tasks::get(&pool, &id).await.unwrap().unwrap().status,
            status
        );
    }
}

#[tokio::test]
async fn the_park_sweep_names_what_it_could_not_check() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;

    let unloadable = open_task_at(&pool, &project_id, "vanished-flow", &dir, "coding").await;
    let missing_stage = open_task_at(&pool, &project_id, &def.name, &dir, "ghost").await;
    let no_session = open_task_at(&pool, &project_id, &def.name, &dir, "coding").await;

    let fresh_binary = named_script_binary(
        &dir,
        "fake-claude-fresh",
        json!([
            {"op": "read_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    );
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), &fresh_binary, &dir);
    let report = engine.park_interrupted_turns().await.unwrap();
    assert_eq!(
        report,
        ParkReport {
            agent_turns: 1,
            shells: 0,
            stuck_other: 2
        }
    );

    let reason = |id: &str| {
        let pool = pool.clone();
        let id = id.to_string();
        async move {
            let task = tasks::get(&pool, &id).await.unwrap().unwrap();
            assert_eq!(task.status, "stuck");
            task.stuck_reason.unwrap()
        }
    };
    let r = reason(&unloadable).await;
    assert!(
        r.starts_with(
            "the daemon restarted and could not load this task's workflow to check whether \
                 stage 'coding' was interrupted: "
        ) && r.ends_with("; fix the workflow file, then retry"),
        "{r}"
    );
    assert_eq!(
        reason(&missing_stage).await,
        "stage 'ghost' no longer exists in the task's workflow"
    );
    assert_eq!(reason(&no_session).await, agent_reason("coding"));

    // No session to resume: retry starts the stage fresh.
    let outcome = engine
        .retry_task(&no_session, RetryMode::Auto)
        .await
        .unwrap();
    assert!(
        !outcome.resumed && outcome.fresh_reason.is_some(),
        "{outcome:?}"
    );
    wait_until_task_status(&pool, &no_session, "closed").await;
}

/// A single-shot session that ends `daemon_stopped` while its process
/// is still being watched parks the task rather than advancing it. Run
/// through the real shutdown path.
#[tokio::test]
async fn the_watcher_parks_a_turn_that_ended_daemon_stopped() {
    use std::os::unix::fs::PermissionsExt;

    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let wrapper = dir.join("fake-claude-spawns-child");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nCHOCO_TEST_HEARTBEAT='{}' CHOCO_TEST_CHILD_PID='{}' exec '{}' \"$@\"\n",
            dir.join("heartbeat").display(),
            dir.join("child.pid").display(),
            fixture_binary("fake_claude_spawns_child.py"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let events_notify = Arc::new(Notify::new());
    let adapter: Arc<dyn AgentAdapter> =
        Arc::new(ClaudeAdapter::with_binary(wrapper.display().to_string()));
    let manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new(
        pool.clone(),
        Arc::clone(&manager),
        dir.to_path_buf(),
        None,
        events_notify,
    );
    engine.start_task(&task_id, &def, None).await.unwrap();
    let run = crate::test_support::wait_until("the turn's session", || async {
        match runs_for_stage(&pool, &task_id, "coding").await.pop() {
            Some(run) if run.adapter_session_id.is_some() => Ok(run),
            other => Err(format!("{other:?}")),
        }
    })
    .await;

    manager.shutdown(Duration::from_secs(10)).await;

    wait_until_task_status(&pool, &task_id, "stuck").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.stuck_reason.unwrap(), agent_reason("coding"));
    assert_eq!(
        workflow_state::get(&pool, &task_id)
            .await
            .unwrap()
            .unwrap()
            .current_stage,
        "coding"
    );
    let ended = sessions::get(&pool, &run.id).await.unwrap().unwrap();
    assert_eq!(ended.end_reason, Some(SessionEndReason::DaemonStopped));
}

#[tokio::test]
async fn in_flight_lists_only_what_a_restart_would_strand() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("coder-turn.md"), "x").unwrap();
    std::fs::write(dir.join("mixed-flow.yaml"), MIXED_FLOW).unwrap();
    std::fs::write(dir.join("chat-flow.yaml"), CHAT_FLOW).unwrap();
    write_poll_flow(&dir, "poll-flow", "echo PENDING", Some("1h"), false);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;

    let turn = open_task_at(&pool, &project_id, "mixed-flow", &dir, "turn").await;
    let sh = open_task_at(&pool, &project_id, "mixed-flow", &dir, "sh").await;
    let unloadable = open_task_at(&pool, &project_id, "vanished-flow", &dir, "turn").await;
    let missing = open_task_at(&pool, &project_id, "mixed-flow", &dir, "ghost").await;
    for (flow, stage) in [
        ("poll-flow", "watch"),
        ("poll-flow", "gate"),
        ("chat-flow", "chatting"),
    ] {
        open_task_at(&pool, &project_id, flow, &dir, stage).await;
    }
    let closed = open_task_at(&pool, &project_id, "mixed-flow", &dir, "turn").await;
    tasks::update_status(&pool, &closed, "closed")
        .await
        .unwrap();

    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let mut listed: Vec<(String, String, String)> = engine
        .in_flight()
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.task_id, f.stage, f.kind))
        .collect();
    listed.sort();
    let mut expected = vec![
        (turn, "turn".to_string(), "agent_turn".to_string()),
        (sh, "sh".to_string(), "shell".to_string()),
        (unloadable, "turn".to_string(), "unknown".to_string()),
        (missing, "ghost".to_string(), "unknown".to_string()),
    ];
    expected.sort();
    assert_eq!(listed, expected);
}

// ---- #175: a human_gate that watches for its answer ----

const TWO_MARKERS: &str = r#"    markers:
      - line: /request-changes
        then: changes_requested
      - line: /approve
        then: approved
"#;

fn count_lines(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |s| s.lines().count())
}

async fn wait_until_count(path: &Path, at_least: usize) {
    crate::test_support::wait_until(
        &format!("{} lines in {}", at_least, path.display()),
        || async {
            let n = count_lines(path);
            if n >= at_least {
                Ok(())
            } else {
                Err(format!("{n} lines"))
            }
        },
    )
    .await
}

/// `gate` is the entry stage; `body` is its YAML (indented under `gate:`).
/// `approved`, `changes_requested`, `timeout` and `resumed` go to terminal
/// stages, so nothing after the gate starts a runner.
fn write_gate_flow(dir: &Path, name: &str, body: &str) -> Arc<WorkflowDefinition> {
    let yaml = format!(
        r#"
name: {name}
stages:
  gate:
    kind: human_gate
{body}
  done:
    kind: terminal
  timed_out:
    kind: terminal
"#
    );
    std::fs::write(dir.join(format!("{name}.yaml")), &yaml).unwrap();
    Arc::new(WorkflowDefinition::parse(&yaml, dir).unwrap())
}

/// A watcher whose command logs each run to `counter` and never matches.
fn counting_watch(counter: &Path, extra: &str) -> String {
    format!(
        r#"    watch:
      command: "echo x >> {}; echo WAITING"
      interval: 1s
{extra}      outcomes:
        - match: "NEVER_MATCHES_XYZ"
          then: approved
"#,
        counter.display()
    )
}

/// The watching gate with the two markers, `capture: text`.
fn marker_gate_flow(dir: &Path, name: &str, counter: &Path) -> Arc<WorkflowDefinition> {
    let body = format!(
        "    capture: text\n{TWO_MARKERS}{}    on: {{ approved: done, changes_requested: done, timeout: timed_out }}",
        counting_watch(counter, "")
    );
    write_gate_flow(dir, name, &body)
}

async fn human_messages(pool: &SqlitePool, task_id: &str) -> Vec<Value> {
    events::list_for_task(pool, task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::HumanMessage)
        .map(|e| e.payload)
        .collect()
}

#[tokio::test]
async fn a_watching_gate_advances_on_its_watchers_outcome_and_captures_the_output() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_gate_flow(
        &dir,
        "gate-go",
        r#"    capture: text
    watch:
      command: "echo GO"
      interval: 1s
      outcomes:
        - match: "GO"
          then: go
    on: { go: done }"#,
    );
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "closed").await;

    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "done");
    assert_eq!(state.payload["stages"]["gate"], json!("GO"));
    let trail: Vec<String> = stage_trail(&pool, &task_id)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(trail, ["gate", "done"]);
}

#[tokio::test]
async fn a_marked_reply_resolves_a_watching_gate_and_stops_its_watcher() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let def = marker_gate_flow(&dir, "gate-reply", &counter);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&counter, 1).await;
    assert!(engine.has_detached_runner(&task_id));

    engine
        .send_message_or_resume(&task_id, "Looks good.\n/approve")
        .await
        .unwrap();

    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "done");
    assert_eq!(state.stage_kind.as_deref(), Some("terminal"));
    assert_eq!(state.payload["stages"]["gate"], json!("Looks good."));
    let messages = human_messages(&pool, &task_id).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["text"], json!("Looks good.\n/approve"));
    assert_eq!(messages[0]["outcome"], json!("approved"));
    assert!(!engine.has_detached_runner(&task_id));
    let before = count_lines(&counter);
    tokio::time::sleep(StdDuration::from_secs(3)).await;
    assert_eq!(count_lines(&counter), before, "the watcher kept running");
}

#[tokio::test]
async fn a_refused_reply_changes_nothing_and_the_watcher_keeps_running() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let def = marker_gate_flow(&dir, "gate-refuse", &counter);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&counter, 1).await;

    for (text, conflict) in [("looks fine", false), ("/approve\n/request-changes", true)] {
        let payload_before = payload_of(&pool, &task_id).await;
        let messages_before = human_messages(&pool, &task_id).await.len();
        let err = engine
            .send_message_or_resume(&task_id, text)
            .await
            .unwrap_err();
        match (conflict, &err) {
            (false, SendMessageOrResumeError::ReplyNeedsMarker { stage, markers }) => {
                assert_eq!(stage, "gate");
                assert_eq!(markers, &["/request-changes", "/approve"]);
            }
            (true, SendMessageOrResumeError::ReplyHasConflictingMarkers { stage, found }) => {
                assert_eq!(stage, "gate");
                assert_eq!(found, &["/request-changes", "/approve"]);
            }
            _ => panic!("unexpected error for {text:?}: {err:?}"),
        }
        assert!(err.to_string().ends_with("Nothing was sent."));

        let state = state_of(&pool, &task_id).await;
        assert_eq!(state.current_stage, "gate");
        assert_eq!(
            tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
            "open"
        );
        assert!(engine.has_detached_runner(&task_id));
        assert_eq!(human_messages(&pool, &task_id).await.len(), messages_before);
        assert_eq!(payload_of(&pool, &task_id).await, payload_before);
        let seen = count_lines(&counter);
        wait_until_count(&counter, seen + 1).await;
    }
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn a_gate_without_markers_still_resumes_on_resumed_with_the_text_verbatim() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_gate_flow(
        &dir,
        "gate-plain",
        "    capture: text\n    on: { resumed: done }",
    );
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();

    engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap();

    let state = state_of(&pool, &task_id).await;
    assert_eq!(state.current_stage, "done");
    assert_eq!(state.payload["stages"]["gate"], json!("/approve"));
    let messages = human_messages(&pool, &task_id).await;
    assert_eq!(messages[0]["text"], json!("/approve"));
    assert_eq!(messages[0]["outcome"], json!("resumed"));
}

fn timeout_gate_flow(dir: &Path, name: &str, counter: &Path) -> Arc<WorkflowDefinition> {
    let body = format!(
        "    capture: text\n{}    on: {{ approved: done, timeout: timed_out }}",
        counting_watch(counter, "      timeout: 1h\n")
    );
    write_gate_flow(dir, name, &body)
}

#[tokio::test]
async fn a_restart_at_a_watching_gate_advances_on_timeout_when_the_deadline_passed() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let def = timeout_gate_flow(&dir, "gate-expired", &counter);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    seed_row(
        &pool,
        &task_id,
        "gate",
        json!({ "poll_window": window_json("gate", now - chrono::Duration::hours(2), Some(now - chrono::Duration::hours(1))) }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.resumed, 1);
    wait_until_stage(&pool, &task_id, "timed_out").await;
    assert_eq!(count_lines(&counter), 0, "an expired deadline runs nothing");
}

#[tokio::test]
async fn a_restart_at_a_watching_gate_keeps_the_stored_deadline() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let def = timeout_gate_flow(&dir, "gate-stored", &counter);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    let window = window_json(
        "gate",
        now - chrono::Duration::minutes(30),
        Some(now + chrono::Duration::seconds(2)),
    );
    seed_row(
        &pool,
        &task_id,
        "gate",
        json!({ "poll_window": window.clone() }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.resumed, 1);
    assert_eq!(
        state_of(&pool, &task_id).await.payload["poll_window"],
        window
    );
    // The configured timeout is 1h: only the stored deadline can end this.
    wait_until_stage(&pool, &task_id, "timed_out").await;
}

async fn upgrade_case(null_kind: bool) {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = r#"
name: upgrade-flow
stages:
  gate:
    kind: human_gate
    on: { resumed: watch }
  watch:
    kind: human_gate
    capture: text
    watch:
      command: "echo GREEN"
      interval: 1s
      timeout: 1h
      outcomes:
        - match: "GREEN"
          then: green
    on: { green: finished, timeout: stalled, error: stalled, again: watch }
  finished:
    kind: terminal
  stalled:
    kind: human_gate
    on: { resumed: finished }
"#;
    std::fs::write(dir.join("upgrade-flow.yaml"), yaml).unwrap();
    let task_id = seed_task_in(&pool, "upgrade-flow", &dir).await;
    let now = Utc::now();
    let window = window_json(
        "watch",
        now - chrono::Duration::minutes(5),
        Some(now + chrono::Duration::hours(1)),
    );
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": window.clone() }),
    )
    .await;
    if null_kind {
        sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
            .bind(&task_id)
            .execute(&pool)
            .await
            .unwrap();
    } else {
        assert!(
            workflow_state::set_stage_kind(&pool, &task_id, "poll")
                .await
                .unwrap()
        );
    }
    let before = state_of(&pool, &task_id).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(
        report,
        PollSweepReport {
            resumed: 1,
            already_running: 0,
            stuck: 0,
            stage_kind_unrecorded: 0,
        }
    );
    let after = state_of(&pool, &task_id).await;
    assert_eq!(after.stage_kind.as_deref(), Some("human_gate"));
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.payload["poll_window"], window);
    wait_until_task_status(&pool, &task_id, "closed").await;
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "finished");
}

#[tokio::test]
async fn a_poll_stage_turned_into_a_watching_gate_resumes_and_records_its_kind() {
    upgrade_case(false).await;
}

#[tokio::test]
async fn a_row_from_before_the_stage_kind_column_is_filled_in_by_the_sweep() {
    upgrade_case(true).await;
}

#[tokio::test]
async fn a_failed_stage_kind_write_in_the_sweep_is_counted_and_the_resume_goes_on() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_poll_flow(&dir, "kind-fail", "echo PENDING", Some("6h"), false);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": window_json("watch", now, Some(now + chrono::Duration::hours(1))) }),
    )
    .await;
    // `seed_row` recorded 'agent_turn'; the sweep must correct it to 'poll'.
    sqlx::query(
        "CREATE TRIGGER fail_kind BEFORE UPDATE OF stage_kind ON workflow_state
             BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.stage_kind_unrecorded, 1);
    assert_eq!(report.resumed, 1);
    assert_eq!(report.stuck, 0);
    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "open"
    );
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn the_sweep_parks_a_task_whose_stage_lost_its_watcher_with_the_new_wording() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_gate_flow(&dir, "lost-watch", "    on: { resumed: done }");
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    seed_row(
        &pool,
        &task_id,
        "gate",
        json!({ "poll_window": window_json("gate", now, None) }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.stuck, 1);
    let reason = tasks::get(&pool, &task_id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert_eq!(
        reason,
        "stage 'gate' was polling when the daemon stopped, but the workflow no longer gives it a watcher; retry to run it as defined"
    );
    // Its kind was still recorded.
    assert_eq!(
        state_of(&pool, &task_id).await.stage_kind.as_deref(),
        Some("human_gate")
    );
}

#[tokio::test]
async fn retry_records_a_stale_stage_kind_even_when_the_payload_is_unchanged() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_gate_flow(&dir, "retry-kind", "    on: { resumed: done }");
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    // `seed_row` leaves the kind as "agent_turn": stale for a gate.
    seed_row(&pool, &task_id, "gate", json!({})).await;
    tasks::mark_stuck(&pool, &task_id, "test").await.unwrap();
    let before = state_of(&pool, &task_id).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    let after = state_of(&pool, &task_id).await;
    assert_eq!(after.stage_kind.as_deref(), Some("human_gate"));
    assert_eq!(after.payload, before.payload);

    // A NULL kind (a row from before the column) is filled in the same way.
    sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    tasks::mark_stuck(&pool, &task_id, "test").await.unwrap();
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert_eq!(
        state_of(&pool, &task_id).await.stage_kind.as_deref(),
        Some("human_gate")
    );
}

#[tokio::test]
async fn the_sweep_parks_a_polling_task_whose_stage_is_gone_from_the_workflow() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_gate_flow(&dir, "gone-stage", "    on: { resumed: done }");
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    seed_row(
        &pool,
        &task_id,
        "removed",
        json!({ "poll_window": window_json("removed", Utc::now(), None) }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    let report = engine.resume_interrupted_polls().await.unwrap();
    assert_eq!(report.stuck, 1);
    let reason = tasks::get(&pool, &task_id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert_eq!(
        reason,
        "stage 'removed' was polling when the daemon stopped, but the workflow no longer defines it; retry to run it as defined"
    );
}

#[tokio::test]
async fn a_watching_gate_without_markers_or_a_resumed_edge_keeps_its_watcher_on_a_refused_reply() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let body = format!(
        "{}    on: {{ approved: done }}",
        counting_watch(&counter, "")
    );
    let def = write_gate_flow(&dir, "gate-no-resumed", &body);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&counter, 1).await;

    let err = engine
        .send_message_or_resume(&task_id, "anything")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SendMessageOrResumeError::Advance(EngineError::UnknownOutcome { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "open"
    );
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "gate");
    assert!(engine.has_detached_runner(&task_id));
    let seen = count_lines(&counter);
    wait_until_count(&counter, seen + 1).await;
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn a_reply_that_fails_before_the_abort_stops_the_watcher_when_it_marks_the_task_stuck() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    // approved -> next, a poll whose timeout overflows: the advance fails
    // with a PollWindow error before the old abort point.
    let yaml = format!(
        r#"
name: gate-fail-early
stages:
  gate:
    kind: human_gate
    capture: text
{TWO_MARKERS}{}    on: {{ approved: next, changes_requested: next }}
  next:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: 3000000000h
    outcomes:
      - match: PENDING
        then: done
    on: {{ done: done, timeout: done }}
  done:
    kind: terminal
"#,
        counting_watch(&counter, "")
    );
    std::fs::write(dir.join("gate-fail-early.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&counter, 1).await;

    let err = engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SendMessageOrResumeError::Advance(EngineError::PollWindow { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
        "stuck"
    );
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "gate");
    // Nothing runs for the stuck gate: the watcher is gone and stays gone.
    assert!(!engine.has_detached_runner(&task_id));
    assert_eq!(runner_slots(&engine, &task_id), 0);
    let before = count_lines(&counter);
    tokio::time::sleep(StdDuration::from_secs(3)).await;
    assert_eq!(count_lines(&counter), before, "the watcher kept running");
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "gate");

    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    wait_until_count(&counter, before + 1).await;
    assert_eq!(runner_slots(&engine, &task_id), 1);
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn cancelling_a_watching_gate_stops_the_watcher_and_refuses_later_replies() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let counter = dir.join("count");
    let def = marker_gate_flow(&dir, "gate-cancel", &counter);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&counter, 1).await;

    engine.cancel_task(&task_id, false).await.unwrap();
    assert!(!engine.has_detached_runner(&task_id));
    let before = count_lines(&counter);
    tokio::time::sleep(StdDuration::from_secs(3)).await;
    assert_eq!(count_lines(&counter), before);
    let err = engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap_err();
    assert!(matches!(err, SendMessageOrResumeError::TaskCancelled));
}

#[tokio::test]
async fn retrying_a_watching_gate_whose_command_could_not_start_restamps_and_restarts_the_watcher()
{
    use std::os::unix::fs::PermissionsExt;
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let script = dir.join("check.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho ran >> \"$(dirname \"$0\")/ran\"\necho WAITING\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    let def = write_gate_flow(
        &dir,
        "gate-retry",
        r#"    capture: text
    watch:
      script_file: check.sh
      interval: 1s
      timeout: 1h
      outcomes:
        - match: "NEVER_MATCHES_XYZ"
          then: approved
    on: { approved: done, timeout: timed_out }"#,
    );
    let task_id = seed_task_in(&pool, &def.name, &dir).await;

    let offset = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let clock_offset = Arc::clone(&offset);
    let events_notify = Arc::new(Notify::new());
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary("unused"));
    let session_manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new_with_clock(
        pool.clone(),
        session_manager,
        dir.to_path_buf(),
        None,
        events_notify,
        Arc::new(move || {
            Utc::now()
                + chrono::Duration::seconds(clock_offset.load(std::sync::atomic::Ordering::SeqCst))
        }),
    );
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_task_status(&pool, &task_id, "stuck").await;
    let first = poll_window_for(&state_of(&pool, &task_id).await.payload, "gate")
        .unwrap()
        .unwrap();

    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    offset.store(100, std::sync::atomic::Ordering::SeqCst);
    engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();

    let second = poll_window_for(&state_of(&pool, &task_id).await.payload, "gate")
        .unwrap()
        .unwrap();
    assert!(second.entered_at > first.entered_at);
    wait_until_count(&dir.join("ran"), 1).await;
    assert!(engine.has_detached_runner(&task_id));
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn stage_kind_follows_the_stage() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let agent_yaml = r#"
name: kind-agent
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  chat:
    kind: agent_turn
    role: coder
    on: {}
"#;
    let agent_def = Arc::new(WorkflowDefinition::parse(agent_yaml, &dir).unwrap());
    let agent_task = seed_task_in(&pool, &agent_def.name, &dir).await;
    let engine = engine_with_adapter(pool.clone(), &fixture_binary("fake_claude.py"));
    engine
        .start_task(&agent_task, &agent_def, Some("hello"))
        .await
        .unwrap();
    assert_eq!(
        state_of(&pool, &agent_task).await.stage_kind.as_deref(),
        Some("agent_turn")
    );

    let yaml = r#"
name: kind-chain
stages:
  prep:
    kind: shell
    command: "true"
    on: { done: gate }
  gate:
    kind: human_gate
    on: { resumed: done }
  done:
    kind: terminal
"#;
    std::fs::write(dir.join("kind-chain.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    assert_eq!(
        state_of(&pool, &task_id).await.stage_kind.as_deref(),
        Some("shell")
    );
    wait_until_stage(&pool, &task_id, "gate").await;
    assert_eq!(
        state_of(&pool, &task_id).await.stage_kind.as_deref(),
        Some("human_gate")
    );
    engine.send_message_or_resume(&task_id, "go").await.unwrap();
    assert_eq!(
        state_of(&pool, &task_id).await.stage_kind.as_deref(),
        Some("terminal")
    );
}

#[test]
fn the_conflict_message_lists_two_markers_with_both_and_three_with_commas() {
    let two = SendMessageOrResumeError::ReplyHasConflictingMarkers {
        stage: "gate".into(),
        found: vec!["/a".into(), "/b".into()],
    };
    assert_eq!(
        two.to_string(),
        "your reply has both /a and /b; keep one. Nothing was sent."
    );
    let three = SendMessageOrResumeError::ReplyHasConflictingMarkers {
        stage: "gate".into(),
        found: vec!["/a".into(), "/b".into(), "/c".into()],
    };
    assert_eq!(
        three.to_string(),
        "your reply has /a, /b and /c; keep one. Nothing was sent."
    );
    let needs = SendMessageOrResumeError::ReplyNeedsMarker {
        stage: "gate".into(),
        markers: vec!["/request-changes".into(), "/approve".into()],
    };
    assert_eq!(
        needs.to_string(),
        "stage 'gate' reads its verdict from your reply: put /request-changes or /approve alone on its own line. Nothing was sent."
    );
}

// ---- Consecutive-rejection loop guards (#184) ----

/// `coding-task`'s shape with every stage a `human_gate`, so `advance`
/// drives it without spawning anything.
fn coding_task_shape_def() -> Arc<WorkflowDefinition> {
    let yaml = r#"
name: coding-task-shape
stages:
  coding:
    kind: human_gate
    on: { resumed: internal_review }
  revising:
    kind: human_gate
    on: { resumed: internal_review }
  internal_review:
    kind: human_gate
    on: { approved: open_pr, changes_requested: revising }
    loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }
  open_pr:
    kind: human_gate
    on: { done: checks_polling, error: escalate_to_human }
  checks_polling:
    kind: human_gate
    on: { green: awaiting_human_review, red: revising, timeout: awaiting_human_review }
    loop_guard: { on: red, max: 3, then: escalate_to_human }
  awaiting_human_review:
    kind: human_gate
    on: { approved: done, changes_requested: revising, timeout: escalate_to_human }
    loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }
  escalate_to_human:
    kind: human_gate
    on: { resumed: revising }
  done:
    kind: terminal
"#;
    Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap())
}

async fn shape_task() -> (
    SqlitePool,
    Arc<WorkflowDefinition>,
    Arc<WorkflowEngine>,
    String,
) {
    let pool = connect_in_memory().await.unwrap();
    let def = coding_task_shape_def();
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    (pool, def, engine, task_id)
}

async fn go(
    engine: &Arc<WorkflowEngine>,
    task_id: &str,
    def: &Arc<WorkflowDefinition>,
    outcomes: &[&str],
) {
    for outcome in outcomes {
        engine.advance(task_id, def, outcome).await.unwrap();
    }
}

async fn counters(pool: &SqlitePool, task_id: &str) -> (String, Value) {
    let state = workflow_state::get(pool, task_id).await.unwrap().unwrap();
    (state.current_stage, state.loop_counters)
}

/// The #172 sequence: two internal rejections, an approval, a human
/// `/request-changes`, then two more rejections stay in the loop.
#[tokio::test]
async fn internal_rejections_do_not_carry_across_a_human_review_round() {
    let (pool, def, engine, id) = shape_task().await;
    go(&engine, &id, &def, &["resumed"]).await;
    go(
        &engine,
        &id,
        &def,
        &[
            "changes_requested",
            "resumed",
            "changes_requested",
            "resumed",
        ],
    )
    .await;
    let (_, c) = counters(&pool, &id).await;
    assert_eq!(c["internal_review"], json!({ "count": 2 }));

    go(&engine, &id, &def, &["approved"]).await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "open_pr");
    assert!(c.get("internal_review").is_none());

    go(&engine, &id, &def, &["done", "green", "changes_requested"]).await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "revising");
    assert_eq!(c["awaiting_human_review"], json!({ "count": 1 }));

    go(
        &engine,
        &id,
        &def,
        &[
            "resumed",
            "changes_requested",
            "resumed",
            "changes_requested",
            "resumed",
        ],
    )
    .await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "internal_review");
    assert_eq!(c["internal_review"], json!({ "count": 2 }));

    let trail: Vec<String> = stage_trail(&pool, &id)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(!trail.iter().any(|s| s == "escalate_to_human"));
    assert_eq!(
        trail,
        [
            "coding",
            "internal_review",
            "revising",
            "internal_review",
            "revising",
            "internal_review",
            "open_pr",
            "checks_polling",
            "awaiting_human_review",
            "revising",
            "internal_review",
            "revising",
            "internal_review",
            "revising",
            "internal_review",
        ]
    );
}

#[tokio::test]
async fn four_internal_rejections_in_a_row_still_escalate() {
    let (pool, def, engine, id) = shape_task().await;
    go(&engine, &id, &def, &["resumed"]).await;
    for n in 1..=3u64 {
        go(&engine, &id, &def, &["changes_requested"]).await;
        let (stage, c) = counters(&pool, &id).await;
        assert_eq!(stage, "revising");
        assert_eq!(c["internal_review"], json!({ "count": n }));
        go(&engine, &id, &def, &["resumed"]).await;
    }
    go(&engine, &id, &def, &["changes_requested"]).await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "escalate_to_human");
    assert!(c.get("internal_review").is_none());
    let trail = stage_trail(&pool, &id).await;
    assert_eq!(
        trail.last().unwrap(),
        &("escalate_to_human".to_string(), json!("changes_requested"))
    );
}

#[tokio::test]
async fn four_red_ci_results_in_a_row_escalate() {
    let (pool, def, engine, id) = shape_task().await;
    go(&engine, &id, &def, &["resumed", "approved", "done"]).await;
    for n in 1..=3u64 {
        go(&engine, &id, &def, &["red"]).await;
        let (stage, c) = counters(&pool, &id).await;
        assert_eq!(stage, "revising");
        assert_eq!(c["checks_polling"], json!({ "count": n }));
        go(&engine, &id, &def, &["resumed", "approved", "done"]).await;
    }
    go(&engine, &id, &def, &["red"]).await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "escalate_to_human");
    assert!(c.get("checks_polling").is_none());
    let trail = stage_trail(&pool, &id).await;
    assert_eq!(
        trail.last().unwrap(),
        &("escalate_to_human".to_string(), json!("red"))
    );
}

#[tokio::test]
async fn a_green_ci_result_resets_the_red_count() {
    let (pool, def, engine, id) = shape_task().await;
    go(&engine, &id, &def, &["resumed", "approved", "done"]).await;
    for _ in 0..2 {
        go(&engine, &id, &def, &["red", "resumed", "approved", "done"]).await;
    }
    go(&engine, &id, &def, &["green"]).await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "awaiting_human_review");
    assert!(c.get("checks_polling").is_none());

    go(
        &engine,
        &id,
        &def,
        &[
            "changes_requested",
            "resumed",
            "approved",
            "done",
            "red",
            "resumed",
            "approved",
            "done",
            "red",
        ],
    )
    .await;
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "revising");
    assert_eq!(c["checks_polling"], json!({ "count": 2 }));
    let trail = stage_trail(&pool, &id).await;
    assert!(!trail.iter().any(|(s, _)| s == "escalate_to_human"));
}

/// The reset is part of the transition's one write: a single read sees
/// the new stage, the removed counter and the arrival together.
#[tokio::test]
async fn the_reset_rides_in_the_transitions_single_write() {
    let (pool, def, engine, id) = shape_task().await;
    go(
        &engine,
        &id,
        &def,
        &[
            "resumed",
            "changes_requested",
            "resumed",
            "changes_requested",
            "resumed",
        ],
    )
    .await;
    let (_, c) = counters(&pool, &id).await;
    assert_eq!(c["internal_review"], json!({ "count": 2 }));
    let before = stage_trail(&pool, &id).await.len();

    engine.advance(&id, &def, "approved").await.unwrap();
    let state = workflow_state::get(&pool, &id).await.unwrap().unwrap();
    assert_eq!(state.current_stage, "open_pr");
    assert!(state.loop_counters.get("internal_review").is_none());
    assert_eq!(
        state.payload["arrival"],
        json!({ "from": "internal_review", "outcome": "approved" })
    );
    assert_eq!(stage_trail(&pool, &id).await.len(), before + 1);
}

#[tokio::test]
async fn an_unknown_outcome_resets_nothing() {
    let (pool, def, engine, id) = shape_task().await;
    go(
        &engine,
        &id,
        &def,
        &[
            "resumed",
            "changes_requested",
            "resumed",
            "changes_requested",
            "resumed",
        ],
    )
    .await;
    let err = engine.advance(&id, &def, "bogus").await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::UnknownOutcome { stage, outcome }
            if stage == "internal_review" && outcome == "bogus"
    ));
    let (stage, c) = counters(&pool, &id).await;
    assert_eq!(stage, "internal_review");
    assert_eq!(c["internal_review"], json!({ "count": 2 }));
}

// ---- #166: a role's `cli:` picks the adapter; an unknown one is rejected ----

/// Like [`engine_with_global_config`], over an arbitrary registry.
fn engine_with_registry(
    pool: SqlitePool,
    registry: Registry,
    workflows_dir: &Path,
    global_config_path: Option<&Path>,
) -> Arc<WorkflowEngine> {
    let events_notify = Arc::new(Notify::new());
    let session_manager = SessionManager::new(
        pool.clone(),
        registry,
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    WorkflowEngine::new(
        pool,
        session_manager,
        workflows_dir.to_path_buf(),
        global_config_path.map(Path::to_path_buf),
        events_notify,
    )
}

fn claude_only_registry() -> Registry {
    Registry::single(Arc::new(ClaudeAdapter::with_binary(fixture_binary(
        "fake_claude.py",
    ))))
}

fn write_cli_workflow(dir: &Path, file: &str, cli_line: &str) -> PathBuf {
    fs::write(dir.join("coder-turn.md"), "implement the thing").unwrap();
    let path = dir.join(file);
    fs::write(
        &path,
        format!(
            "name: cli-flow\nroles:\n  coder:\n{cli_line}    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: coder-turn.md\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
        ),
    )
    .unwrap();
    path
}

const UNKNOWN_CLUADE: &str =
    "role 'coder' uses cli 'cluade', which this daemon doesn't know; known CLIs: claude";

#[test]
fn load_workflow_file_checks_every_roles_cli_against_the_registry() {
    let dir = tempdir();
    let registry = claude_only_registry();
    let bad = write_cli_workflow(&dir, "bad.yaml", "    cli: cluade\n");
    let err = load_workflow_file(&bad, &registry).unwrap_err();
    assert!(matches!(err, WorkflowDefError::UnknownCli(_)), "{err:?}");
    assert_eq!(err.to_string(), UNKNOWN_CLUADE);

    let none = write_cli_workflow(&dir, "none.yaml", "");
    assert!(load_workflow_file(&none, &registry).is_ok());
    let claude = write_cli_workflow(&dir, "claude.yaml", "    cli: claude\n");
    assert!(load_workflow_file(&claude, &registry).is_ok());

    // The valid set is the registry's own keys.
    let fake = RecordingAdapter::new("fake", &fixture_binary("fake_claude.py"));
    let wide = Registry::new(vec![
        Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"))),
        fake,
    ]);
    let fake_flow = write_cli_workflow(&dir, "fake.yaml", "    cli: fake\n");
    assert!(load_workflow_file(&fake_flow, &wide).is_ok());
    assert!(load_workflow_file(&fake_flow, &registry).is_err());
}

#[test]
fn every_builtin_workflow_loads_with_the_production_registry() {
    let root = tempdir();
    let dir = root.join(".builtin-workflows");
    config_root::materialize_builtins(&dir).unwrap();
    let registry = Registry::single(Arc::new(ClaudeAdapter::new()));
    let mut seen = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "yaml") {
            load_workflow_file(&path, &registry)
                .unwrap_or_else(|e| panic!("{} failed to load: {e}", path.display()));
            seen += 1;
        }
    }
    assert!(seen > 0, "no built-in workflows found in {}", dir.display());
}

#[tokio::test]
async fn create_task_from_a_workflow_with_an_unknown_cli_creates_nothing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let bad = write_cli_workflow(&dir, "bad.yaml", "    cli: cluade\n");
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), claude_only_registry(), &dir, None);
    let err = engine
        .create_task_from(&project_id, WorkflowRef::File(bad), "T", "go", json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            CreateTaskError::WorkflowDef(WorkflowDefError::UnknownCli(_))
        ),
        "{err:?}"
    );
    assert_eq!(err.to_string(), UNKNOWN_CLUADE);
    let listed = tasks::list(&pool, Some(&project_id), None).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn create_task_with_an_unknown_cli_in_its_config_creates_nothing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let good = write_cli_workflow(&dir, "good.yaml", "    cli: claude\n");
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), claude_only_registry(), &dir, None);
    let err = engine
        .create_task_from(
            &project_id,
            WorkflowRef::File(good),
            "T",
            "go",
            json!({"roles": {"coder": {"cli": "nope"}}}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::UnknownCli(_)), "{err:?}");
    assert!(err.to_string().contains("'nope'"));
    let listed = tasks::list(&pool, Some(&project_id), None).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn a_legacy_task_whose_workflow_has_an_unknown_cli_fails_to_load() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    write_cli_workflow(&dir, "cli-flow.yaml", "    cli: cluade\n");
    let task_id = seed_task(&pool, "cli-flow").await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(task.workflow_path.is_none());
    let engine = engine_with_registry(pool.clone(), claude_only_registry(), &dir, None);
    let err = engine.load_task_workflow(&task).await.unwrap_err();
    assert!(
        matches!(
            &err,
            LoadTaskWorkflowError::WorkflowDef(WorkflowDefError::UnknownCli(_))
        ),
        "{err:?}"
    );
}

/// The global config can change after the daemon started and validated it.
/// The turn-time lookup is the backstop: the task goes stuck with the
/// message, no session row exists, and nothing was spawned.
#[tokio::test]
async fn a_cli_that_slips_in_after_startup_fails_the_turn_closed() {
    use std::os::unix::fs::PermissionsExt;

    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let flow = write_cli_workflow(&dir, "flow.yaml", "");
    let global = dir.join("config.yaml");
    fs::write(&global, "roles:\n  coder:\n    cli: bogus\n").unwrap();
    let marker = dir.join("claude-was-run");
    let wrapper = dir.join("claude-wrapper");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ntouch '{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            fixture_binary("fake_claude.py")
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine =
        engine_with_global_config(pool.clone(), &wrapper.display().to_string(), &dir, &global);

    let err = engine
        .create_task_from(&project_id, WorkflowRef::File(flow), "T", "go", json!({}))
        .await
        .unwrap_err();
    let task_id = match &err {
        CreateTaskError::Start {
            task_id,
            source: EngineError::UnknownCli(_),
        } => task_id.clone(),
        other => panic!("expected Start/UnknownCli, got {other:?}"),
    };
    let check = |pool: SqlitePool, task_id: String| {
        let marker = marker.clone();
        async move {
            let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
            assert_eq!(task.status, "stuck");
            let reason = task.stuck_reason.unwrap();
            assert!(reason.contains("'bogus'"), "{reason}");
            assert!(reason.contains("known CLIs: claude"), "{reason}");
            assert!(
                sessions::list_for_task(&pool, &task_id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(!marker.exists(), "claude must not have been spawned");
        }
    };
    check(pool.clone(), task_id.clone()).await;

    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RetryTaskError::Enter(EngineError::UnknownCli(_))),
        "{err:?}"
    );
    check(pool.clone(), task_id).await;
}

fn two_adapter_registry(binary: &str) -> (Registry, Arc<RecordingAdapter>, Arc<RecordingAdapter>) {
    let claude = RecordingAdapter::new("claude", binary);
    let fake = RecordingAdapter::new("fake", binary);
    (
        Registry::new(vec![claude.clone(), fake.clone()]),
        claude,
        fake,
    )
}

/// A task stuck at `coding` after an interrupted turn recorded on
/// `recorded_cli`, whose coder now resolves to `current_cli` (task config).
async fn stuck_after_session_on(
    pool: &SqlitePool,
    dir: &Path,
    recorded_cli: &str,
    current_cli: &str,
) -> (String, Session) {
    let def = coding_workflow(dir);
    let task_id = seed_task(pool, &def.name).await;
    sqlx::query("UPDATE tasks SET config = ?, status = 'stuck' WHERE id = ?")
        .bind(json!({"roles": {"coder": {"cli": current_cli}}}).to_string())
        .bind(&task_id)
        .execute(pool)
        .await
        .unwrap();
    seed_row(pool, &task_id, "coding", json!({})).await;
    let run = sessions::create(
        pool,
        sessions::NewSession {
            task_id: &task_id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    sessions::set_adapter_session_id(pool, &run.id, "S1")
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET cli_adapter = ? WHERE id = ?")
        .bind(recorded_cli)
        .bind(&run.id)
        .execute(pool)
        .await
        .unwrap();
    let run = sessions::update_status(
        pool,
        &run.id,
        SessionStatus::Exited,
        Some(Utc::now()),
        Some(SessionEndReason::Interrupted),
    )
    .await
    .unwrap()
    .unwrap();
    (task_id, run)
}

fn finishing_binary(dir: &Path) -> String {
    named_script_binary(
        dir,
        "fake-claude-finish",
        json!([
            {"op": "read_turn"},
            {"op": "report", "outcome": "done"},
            {"op": "result"},
        ]),
    )
}

#[tokio::test]
async fn each_role_runs_on_the_adapter_its_cli_names() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let flows = dir.join("flows");
    fs::create_dir_all(&flows).unwrap();
    write_two_role_workflow(&flows);
    let binary = fixture_binary("fake_claude.py");
    let (registry, claude, fake) = two_adapter_registry(&binary);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), registry, &flows, None);
    let task = engine
        .create_task(
            &project_id,
            "multi-role",
            "T",
            "go",
            json!({"roles": {
                "coder": {"cli": "fake", "model": "m"},
                "reviewer": {"cli": "claude", "model": "m"},
            }}),
        )
        .await
        .unwrap();
    wait_until_stage(&pool, &task.id, "finished").await;
    let coder = wait_until_run_for_stage(&pool, &task.id, "coding").await;
    let reviewer = wait_until_run_for_stage(&pool, &task.id, "internal_review").await;
    assert_eq!(coder.cli_adapter, "fake");
    assert_eq!(reviewer.cli_adapter, "claude");
    assert_eq!(fake.calls(), vec![RecordedCall::Start]);
    assert_eq!(claude.calls(), vec![RecordedCall::Start]);
}

#[tokio::test]
async fn a_retry_resumes_on_the_adapter_that_ran_the_session() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, claude, fake) = two_adapter_registry(&finishing_binary(&dir));
    let (task_id, old) = stuck_after_session_on(&pool, &dir, "fake", "fake").await;
    let engine = engine_with_registry(pool.clone(), registry, &dir, None);
    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(outcome.resumed, "{outcome:?}");
    assert_eq!(
        fake.calls(),
        vec![RecordedCall::Resume {
            adapter_session_id: "S1".to_string()
        }]
    );
    assert!(claude.calls().is_empty());
    let new = run_after(&pool, &task_id, "coding", &old).await;
    assert_eq!(new.cli_adapter, "fake");
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

#[tokio::test]
async fn a_changed_cli_starts_fresh_and_a_demanded_resume_is_refused() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, claude, fake) = two_adapter_registry(&finishing_binary(&dir));
    let (task_id, old) = stuck_after_session_on(&pool, &dir, "claude", "fake").await;
    let engine = engine_with_registry(pool.clone(), registry, &dir, None);

    let err = engine
        .retry_task(&task_id, RetryMode::Resume)
        .await
        .unwrap_err();
    let RetryTaskError::NotResumable(reason) = &err else {
        panic!("expected NotResumable, got {err:?}");
    };
    assert!(
        reason.contains("changed from 'claude' to 'fake'"),
        "{reason}"
    );
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert_eq!(runs_for_stage(&pool, &task_id, "coding").await.len(), 1);
    assert!(fake.calls().is_empty() && claude.calls().is_empty());

    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(!outcome.resumed, "{outcome:?}");
    assert!(
        outcome
            .fresh_reason
            .as_deref()
            .is_some_and(|r| r.contains("changed from 'claude' to 'fake'")),
        "{outcome:?}"
    );
    assert_eq!(fake.calls(), vec![RecordedCall::Start]);
    assert!(claude.calls().is_empty());
    let new = run_after(&pool, &task_id, "coding", &old).await;
    assert_eq!(new.cli_adapter, "fake");
}

#[tokio::test]
async fn a_recorded_adapter_the_daemon_lacks_is_not_resumable() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, claude, fake) = two_adapter_registry(&finishing_binary(&dir));
    let (task_id, old) = stuck_after_session_on(&pool, &dir, "ghost", "claude").await;
    let engine = engine_with_registry(pool.clone(), registry, &dir, None);
    let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
    assert!(!outcome.resumed, "{outcome:?}");
    let why = outcome.fresh_reason.clone().unwrap();
    assert!(
        why.contains("'ghost'") && why.contains("known CLIs"),
        "{why}"
    );
    assert_eq!(claude.calls(), vec![RecordedCall::Start]);
    assert!(fake.calls().is_empty());
    let new = run_after(&pool, &task_id, "coding", &old).await;
    assert_eq!(new.cli_adapter, "claude");
}

#[tokio::test]
async fn a_role_cli_that_cannot_be_resolved_makes_the_session_not_resumable() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = coding_workflow(&dir);
    let (task_id, _old) = stuck_after_session_on(&pool, &dir, "claude", "claude").await;
    // A malformed global config can't be loaded, so the role's cli can't be
    // resolved; the decision says so rather than guessing.
    let global = dir.join("bad-config.yaml");
    fs::write(&global, "roles: [not, a, map").unwrap();
    let (registry, _claude, _fake) = two_adapter_registry(&finishing_binary(&dir));
    let engine = engine_with_registry(pool.clone(), registry, &dir, Some(&global));
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let last = sessions::get_current_for_stage(&pool, &task_id, "coding")
        .await
        .unwrap();
    let why = engine
        .resumable_session(&task, &def, &def.stages["coding"], last.as_ref())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        why.starts_with("the role's CLI could not be resolved:"),
        "{why}"
    );
}

/// The resume path takes the adapter from the recorded session, never from
/// the role's resolved `cli`, even when the two differ.
#[tokio::test]
async fn enter_agent_turn_resumes_on_the_recorded_adapter_not_the_resolved_cli() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, claude, fake) = two_adapter_registry(&finishing_binary(&dir));
    let def = coding_workflow(&dir);
    let (task_id, old) = stuck_after_session_on(&pool, &dir, "fake", "claude").await;
    sqlx::query("UPDATE tasks SET status = 'open' WHERE id = ?")
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    let engine = engine_with_registry(pool.clone(), registry, &dir, None);
    let resume = ResumeSession {
        adapter_session_id: "S1".to_string(),
        cli_adapter: "fake".to_string(),
        previous_session_id: old.id.clone(),
        end_reason: SessionEndReason::Interrupted,
    };
    engine
        .enter_stage(
            &task_id,
            &def,
            "coding",
            None,
            None,
            &json!({}),
            Some(&resume),
        )
        .await
        .unwrap();
    assert_eq!(
        fake.calls(),
        vec![RecordedCall::Resume {
            adapter_session_id: "S1".to_string()
        }]
    );
    assert!(claude.calls().is_empty());
    let new = run_after(&pool, &task_id, "coding", &old).await;
    assert_eq!(new.cli_adapter, "fake");
}

// ---- #166: the omp adapter's memory / skill rule ----

const OMP_MEMORY_REJECTION: &str = "role 'coder' runs on cli 'omp', which can't use memory: true; \
     remove memory: true or run the role on cli: claude";

/// A registry with the fake claude and a real `OmpAdapter` whose binary is a
/// wrapper that leaves a marker if it is ever run.
fn claude_and_omp_registry(dir: &Path) -> (Registry, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let marker = dir.join("omp-was-run");
    let wrapper = dir.join("omp-wrapper");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let registry = Registry::new(vec![
        Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"))),
        Arc::new(crate::adapter::OmpAdapter::with_binary(
            wrapper.display().to_string(),
            dir.join("omp-state"),
        )),
    ]);
    (registry, marker)
}

#[test]
fn load_workflow_file_rejects_memory_and_pattern_skills_on_an_omp_role_only() {
    let dir = tempdir();
    let (registry, _) = claude_and_omp_registry(&dir);
    let omp_memory = write_cli_workflow(&dir, "a.yaml", "    cli: omp\n    memory: true\n");
    let err = load_workflow_file(&omp_memory, &registry).unwrap_err();
    assert!(matches!(err, WorkflowDefError::RoleRejected(_)), "{err:?}");
    assert_eq!(err.to_string(), OMP_MEMORY_REJECTION);

    let omp_skill = write_cli_workflow(&dir, "b.yaml", "    cli: omp\n    skills: [\"deploy*\"]\n");
    let err = load_workflow_file(&omp_skill, &registry).unwrap_err();
    assert!(err.to_string().contains("skill 'deploy*'"), "{err}");

    // Claude with memory, and omp without it, load.
    let claude_memory = write_cli_workflow(&dir, "c.yaml", "    cli: claude\n    memory: true\n");
    assert!(load_workflow_file(&claude_memory, &registry).is_ok());
    let omp_plain = write_cli_workflow(&dir, "d.yaml", "    cli: omp\n    skills: [plain]\n");
    assert!(load_workflow_file(&omp_plain, &registry).is_ok());
    // No `cli:` at all: the check is the turn-start one's job.
    let no_cli = write_cli_workflow(&dir, "e.yaml", "    memory: true\n");
    assert!(load_workflow_file(&no_cli, &registry).is_ok());
}

#[tokio::test]
async fn create_task_pointing_a_memory_role_at_omp_in_its_config_creates_nothing() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, marker) = claude_and_omp_registry(&dir);
    let flow = write_cli_workflow(&dir, "flow.yaml", "    memory: true\n");
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), registry, &dir, None);
    let err = engine
        .create_task_from(
            &project_id,
            WorkflowRef::File(flow.clone()),
            "T",
            "go",
            json!({"roles": {"coder": {"cli": "omp"}}}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    assert_eq!(err.to_string(), OMP_MEMORY_REJECTION);
    assert!(
        tasks::list(&pool, Some(&project_id), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!marker.exists());

    // A role the workflow doesn't define is skipped; claude is accepted.
    let (def, _) = load_workflow_file(&flow, engine.registry()).unwrap();
    for config in [
        json!({"roles": {"nobody": {"cli": "omp"}}}),
        json!({"roles": {"coder": {"cli": "claude"}}}),
        json!({"roles": {"coder": {"cli": 1}}}),
    ] {
        assert!(
            crate::adapter::check_task_config_roles(&config, &def, engine.registry()).is_ok(),
            "{config}"
        );
    }
}

/// The global config isn't visible to any earlier check, so the turn-start
/// one is what stops a memory role routed to omp: the task goes stuck with
/// the message, no session row exists, and nothing was spawned.
#[tokio::test]
async fn a_memory_role_routed_to_omp_by_the_global_config_fails_the_turn_closed() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let (registry, marker) = claude_and_omp_registry(&dir);
    let flow = write_cli_workflow(&dir, "flow.yaml", "    memory: true\n");
    let global = dir.join("config.yaml");
    fs::write(&global, "roles:\n  coder:\n    cli: omp\n").unwrap();
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), registry, &dir, Some(&global));

    let err = engine
        .create_task_from(&project_id, WorkflowRef::File(flow), "T", "go", json!({}))
        .await
        .unwrap_err();
    let task_id = match &err {
        CreateTaskError::Start {
            task_id,
            source: EngineError::RoleRejected(_),
        } => task_id.clone(),
        other => panic!("expected Start/RoleRejected, got {other:?}"),
    };
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert!(
        task.stuck_reason.unwrap().contains(OMP_MEMORY_REJECTION),
        "the stuck reason carries the message"
    );
    assert!(
        sessions::list_for_task(&pool, &task_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!marker.exists(), "omp must not have been started");

    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RetryTaskError::Enter(EngineError::RoleRejected(_))),
        "{err:?}"
    );
}

async fn template_unresolved_events(pool: &SqlitePool, task_id: &str) -> Vec<Value> {
    events::list_for_task(pool, task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::TemplateUnresolved)
        .map(|e| e.payload)
        .collect()
}

/// A reference to a later capturing stage, and a stage's own capture on its
/// first entry, are "not run yet": rendered empty, no event.
#[tokio::test]
async fn references_to_stages_that_have_not_run_record_no_note() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: not-run-yet
stages:
  first:
    kind: shell
    command: "echo [{{ stages.later.x }}] [{{ stages.first }}]"
    capture: text
    on: { done: later }
  later:
    kind: shell
    command: "printf '{\"x\": 1}'"
    capture: json
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

    let ran = wait_until_shell_event_for(&pool, &task_id, "first").await;
    assert_eq!(ran["command"], json!("echo [] []"));
    assert!(template_unresolved_events(&pool, &task_id).await.is_empty());

    // The transitions wrote the marker once per finished stage, in order;
    // the terminal stage never finishes.
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert_eq!(state.payload["finished_stages"], json!(["first", "later"]));
}

/// A capturing stage that finished without storing a capture is a real
/// mismatch: a later reference to it is recorded.
#[tokio::test]
async fn a_stage_that_ran_without_a_capture_is_recorded_when_referenced() {
    let pool = connect_in_memory().await.unwrap();
    let yaml = r#"
name: ran-no-capture
stages:
  gate:
    kind: human_gate
    capture: text
    on: { done: report }
  report:
    kind: shell
    command: "echo {{ stages.gate }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
    let def = Arc::new(WorkflowDefinition::parse(yaml, Path::new(".")).unwrap());
    let task_id = seed_task(&pool, &def.name).await;
    let engine = engine_with_adapter(pool.clone(), "unused");
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "gate").await;
    engine.advance(&task_id, &def, "done").await.unwrap();
    wait_until_stage(&pool, &task_id, "finished").await;

    let notes = template_unresolved_events(&pool, &task_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["stage"], json!("report"));
    assert_eq!(notes[0]["placeholders"], json!(["{{ stages.gate }}"]));
    let state = workflow_state::get(&pool, &task_id).await.unwrap().unwrap();
    assert!(
        state.payload["finished_stages"]
            .as_array()
            .unwrap()
            .contains(&json!("gate"))
    );
}

#[test]
fn a_malformed_finished_stages_value_is_replaced_by_a_list() {
    for bad in [json!("oops"), json!(7), json!({"a": 1})] {
        let mut payload = json!({ "finished_stages": bad, "keep": 1 });
        mark_stage_finished(&mut payload, "gate");
        mark_stage_finished(&mut payload, "gate");
        assert_eq!(payload["finished_stages"], json!(["gate"]));
        assert_eq!(payload["keep"], json!(1));
    }
    let mut not_object = json!("scalar");
    mark_stage_finished(&mut not_object, "gate");
    assert_eq!(not_object["finished_stages"], json!(["gate"]));
}

// ---- backoff, re-watch and marker-only refusal (#179) ----

/// An engine whose clock is the real one plus a movable offset in seconds.
fn engine_with_offset_clock(
    pool: SqlitePool,
    dir: &Path,
) -> (Arc<WorkflowEngine>, Arc<std::sync::atomic::AtomicI64>) {
    let offset = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let clock_offset = Arc::clone(&offset);
    let events_notify = Arc::new(Notify::new());
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary("unused"));
    let session_manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    let engine = WorkflowEngine::new_with_clock(
        pool,
        session_manager,
        dir.to_path_buf(),
        None,
        events_notify,
        Arc::new(move || {
            Utc::now()
                + chrono::Duration::seconds(clock_offset.load(std::sync::atomic::Ordering::SeqCst))
        }),
    );
    (engine, offset)
}

/// An engine whose clock always reads `at`.
fn engine_with_fixed_clock(pool: SqlitePool, dir: &Path, at: DateTime<Utc>) -> Arc<WorkflowEngine> {
    let events_notify = Arc::new(Notify::new());
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary("unused"));
    let session_manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    WorkflowEngine::new_with_clock(
        pool,
        session_manager,
        dir.to_path_buf(),
        None,
        events_notify,
        Arc::new(move || at),
    )
}

/// A poll whose command counts its runs in `counter`, waits for `go` to
/// exist, and prints the count. It matches once the count reaches `target`.
fn write_backoff_poll_flow(
    dir: &Path,
    name: &str,
    target: u32,
    timeout: Option<&str>,
) -> Arc<WorkflowDefinition> {
    let counter = dir.join("count");
    let go = dir.join("go");
    let timeout = timeout
        .map(|t| format!("    timeout: {t}\n"))
        .unwrap_or_default();
    let on_timeout = if timeout.is_empty() {
        ""
    } else {
        ", timeout: stalled"
    };
    let yaml = format!(
        r#"
name: {name}
stages:
  watch:
    kind: poll
    command: 'echo x >> {c}; while [ ! -f {g} ]; do sleep 0.05; done; wc -l < {c}'
    interval: 1h
    backoff:
      - {{ after: 1h, interval: 1s }}
{timeout}    outcomes:
      - match: '\A\s*{target}'
        then: done
    on: {{ done: finished{on_timeout} }}
  finished:
    kind: terminal
  stalled:
    kind: terminal
"#,
        c = counter.display(),
        g = go.display()
    );
    std::fs::write(dir.join(format!("{name}.yaml")), &yaml).unwrap();
    Arc::new(WorkflowDefinition::parse(&yaml, dir).unwrap())
}

#[tokio::test]
async fn a_backoff_step_shortens_the_wait_once_the_clock_reaches_it() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_backoff_poll_flow(&dir, "backoff-live", 2, None);
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let (engine, offset) = engine_with_offset_clock(pool.clone(), &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&dir.join("count"), 1).await;
    // The first attempt is still waiting for `go`; by the time it ends, the
    // stage has been running for two hours.
    offset.store(2 * 3600, std::sync::atomic::Ordering::SeqCst);
    std::fs::write(dir.join("go"), "").unwrap();
    // With the base interval of 1h this would wait an hour.
    wait_until_stage(&pool, &task_id, "finished").await;
}

#[tokio::test]
async fn a_restart_picks_its_backoff_step_from_the_stored_entry_time() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    // Never matches, so the stage stays put and its window can be read.
    let def = write_backoff_poll_flow(&dir, "backoff-restart", 99, Some("200h"));
    std::fs::write(dir.join("go"), "").unwrap();
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let now = Utc::now();
    let deadline = now + chrono::Duration::hours(100);
    seed_row(
        &pool,
        &task_id,
        "watch",
        json!({ "poll_window": window_json("watch", now - chrono::Duration::hours(2), Some(deadline)) }),
    )
    .await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.resume_interrupted_polls().await.unwrap();
    // The second attempt arrives after 1s, not 1h.
    wait_until_count(&dir.join("count"), 2).await;
    let window = poll_window_for(&state_of(&pool, &task_id).await.payload, "watch")
        .unwrap()
        .unwrap();
    assert_eq!(window.deadline, Some(deadline));
    engine.abort_detached_runners(&task_id).await;
}

#[tokio::test]
async fn the_timeout_note_says_how_long_the_stage_watched() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let def = write_backoff_poll_flow(&dir, "backoff-note", 99, Some("90m"));
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let (engine, offset) = engine_with_offset_clock(pool.clone(), &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&dir.join("count"), 1).await;
    offset.store(100 * 60 + 30, std::sync::atomic::Ordering::SeqCst);
    std::fs::write(dir.join("go"), "").unwrap();
    wait_until_stage(&pool, &task_id, "stalled").await;
    let event = wait_until_decisive_poll_event(&pool, &task_id).await;
    assert_eq!(
        event["note"],
        json!("no outcome matched in 1 attempts over 1h40m; timeout elapsed")
    );
}

const REWATCH_FLOW: &str = r#"
name: rw
stages:
  review:
    kind: human_gate
    markers:
      - line: /request-changes
        then: changes_requested
      - line: /approve
        then: approved
    watch:
      command: "echo PENDING"
      interval: 1s
      timeout: 1h
      outcomes:
        - match: 'NEVER_XYZ'
          then: approved
    on: { approved: done, changes_requested: revising, timeout: esc }
  ci:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: 30m
    outcomes:
      - match: 'NEVER_XYZ'
        then: green
    on: { green: done, timeout: esc, other: ci2, more: esc2 }
  ci2:
    kind: poll
    command: "echo PENDING"
    interval: 1s
    timeout: 30m
    outcomes:
      - match: 'NEVER_XYZ'
        then: green
    on: { green: done, timeout: esc_w }
  esc:
    kind: human_gate
    on: { resumed: revising }
  esc2:
    kind: human_gate
    on: { resumed: revising }
  esc_w:
    kind: human_gate
    watch:
      command: "echo PENDING"
      interval: 1s
      outcomes:
        - match: 'NEVER_XYZ'
          then: ok
    on: { resumed: revising, ok: done }
  revising:
    kind: terminal
  done:
    kind: terminal
"#;

async fn rewatch_setup(
    stage: &str,
    arrival: Value,
) -> (
    SqlitePool,
    TempDir,
    String,
    Arc<WorkflowEngine>,
    DateTime<Utc>,
) {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    std::fs::write(dir.join("rw.yaml"), REWATCH_FLOW).unwrap();
    let task_id = seed_task_in(&pool, "rw", &dir).await;
    seed_row(
        &pool,
        &task_id,
        stage,
        json!({ "task": { "title": "T" }, "arrival": arrival }),
    )
    .await;
    sqlx::query("UPDATE workflow_state SET loop_counters = ? WHERE task_id = ?")
        .bind(sqlx::types::Json(json!({ "review": { "count": 2 } })))
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
    let now = Utc::now();
    let engine = engine_with_fixed_clock(pool.clone(), &dir, now);
    (pool, dir, task_id, engine, now)
}

#[tokio::test]
async fn retry_at_a_gate_after_a_watcher_timeout_watches_again() {
    for (from, timeout) in [("review", 3600), ("ci", 1800)] {
        let (pool, _dir, task_id, engine, now) =
            rewatch_setup("esc", json!({ "from": from, "outcome": "timeout" })).await;
        let before = state_of(&pool, &task_id).await;
        let outcome = engine.retry_task(&task_id, RetryMode::Auto).await.unwrap();
        assert_eq!(
            outcome,
            RetryOutcome {
                stage: from.to_string(),
                resumed: false,
                adapter_session_id: None,
                fresh_reason: None,
                rewatched: true,
            }
        );
        wait_for_poll_attempt(&pool, &task_id).await;
        let state = state_of(&pool, &task_id).await;
        assert_eq!(state.current_stage, from);
        assert_eq!(state.loop_counters, before.loop_counters);
        assert_eq!(
            state.payload["arrival"],
            json!({ "from": "esc", "outcome": "retry" })
        );
        let window = poll_window_for(&state.payload, from).unwrap().unwrap();
        assert_eq!(window.entered_at, now);
        assert_eq!(
            window.deadline,
            Some(now + chrono::Duration::seconds(timeout))
        );
        assert_ne!(state.stage_entered_at, before.stage_entered_at);
        assert_eq!(
            tasks::get(&pool, &task_id).await.unwrap().unwrap().status,
            "open"
        );
        assert!(
            stage_trail(&pool, &task_id)
                .await
                .iter()
                .any(|(s, o)| s == from && o == &json!("retry"))
        );
        engine.abort_detached_runners(&task_id).await;
    }
}

#[tokio::test]
async fn retry_refuses_an_open_task_that_did_not_time_out_at_a_gate() {
    let cases = [
        (
            "a loop guard",
            "esc",
            json!({ "from": "review", "outcome": "changes_requested" }),
        ),
        (
            "an error edge",
            "esc",
            json!({ "from": "ci", "outcome": "error" }),
        ),
        (
            "a stage with no watcher",
            "esc",
            json!({ "from": "esc2", "outcome": "timeout" }),
        ),
        (
            "a watcher timing out elsewhere",
            "esc2",
            json!({ "from": "review", "outcome": "timeout" }),
        ),
        (
            "a gate that watches itself",
            "esc_w",
            json!({ "from": "ci2", "outcome": "timeout" }),
        ),
        (
            "a non-gate stage",
            "ci",
            json!({ "from": "review", "outcome": "timeout" }),
        ),
    ];
    for (label, stage, arrival) in cases {
        let (pool, _dir, task_id, engine, _now) = rewatch_setup(stage, arrival).await;
        let before = state_of(&pool, &task_id).await;
        let err = engine
            .retry_task(&task_id, RetryMode::Auto)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::NotStuck(status) if status == "open"),
            "{label}: {err:?}"
        );
        assert_eq!(state_of(&pool, &task_id).await, before, "{label}");
        assert!(!engine.has_detached_runner(&task_id), "{label}");
    }
}

#[tokio::test]
async fn retry_with_a_mode_at_a_timed_out_gate_is_refused() {
    for mode in [RetryMode::Resume, RetryMode::Fresh] {
        let (pool, _dir, task_id, engine, _now) =
            rewatch_setup("esc", json!({ "from": "review", "outcome": "timeout" })).await;
        let before = state_of(&pool, &task_id).await;
        let err = engine.retry_task(&task_id, mode).await.unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::RewatchTakesNoMode { stage, .. } if stage == "review"),
            "{err:?}"
        );
        assert!(err.to_string().contains("without --resume or --fresh"));
        assert_eq!(state_of(&pool, &task_id).await, before);
    }
}

#[tokio::test]
async fn a_closed_task_is_not_retried_even_at_a_timed_out_gate() {
    let (pool, _dir, task_id, engine, _now) =
        rewatch_setup("esc", json!({ "from": "review", "outcome": "timeout" })).await;
    tasks::update_status(&pool, &task_id, "closed")
        .await
        .unwrap();
    let err = engine
        .retry_task(&task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::NotStuck(s) if s == "closed"),
        "{err:?}"
    );
}

#[tokio::test]
async fn status_reports_a_timed_out_watcher_only_for_an_open_parked_task() {
    let (pool, _dir, task_id, engine, _now) =
        rewatch_setup("esc", json!({ "from": "review", "outcome": "timeout" })).await;
    let task = tasks::get(&pool, &task_id).await.unwrap().unwrap();
    let state = state_of(&pool, &task_id).await;
    assert_eq!(
        engine.watch_timed_out(&task, &state).await,
        Some(WatchTimedOutInfo {
            stage: "review".into(),
            timeout_secs: Some(3600),
            resumes_to: Some("revising".into()),
        })
    );
    let mut closed = task.clone();
    closed.status = "closed".into();
    assert_eq!(engine.watch_timed_out(&closed, &state).await, None);
}

#[tokio::test]
async fn a_marker_only_reply_at_a_gate_without_markers_is_refused() {
    for text in [
        "/approve",
        "/request-changes",
        "/approve\n\n",
        "/approve  \r\n/approve",
    ] {
        let (pool, _dir, task_id, engine, _now) =
            rewatch_setup("esc", json!({ "from": "review", "outcome": "timeout" })).await;
        let before = state_of(&pool, &task_id).await;
        let err = engine
            .send_message_or_resume(&task_id, text)
            .await
            .unwrap_err();
        match &err {
            SendMessageOrResumeError::ReplyIsOnlyMarkers {
                stage,
                found,
                resumes_to,
                rewatch,
                ..
            } => {
                assert_eq!(stage, "esc");
                assert_eq!(found.len(), 1, "{text:?}");
                assert_eq!(resumes_to.as_deref(), Some("revising"));
                assert!(*rewatch);
            }
            other => panic!("{text:?}: {other:?}"),
        }
        let message = err.to_string();
        assert!(
            message.contains("doesn't read /")
                && message.contains("sends the task on to stage 'revising'")
                && message.contains(&format!("choco task retry {task_id}")),
            "{message}"
        );
        assert!(human_messages(&pool, &task_id).await.is_empty());
        assert_eq!(state_of(&pool, &task_id).await, before);
    }
}

#[tokio::test]
async fn the_retry_hint_is_left_out_when_the_gate_was_not_reached_by_a_timeout() {
    let (_pool, _dir, task_id, engine, _now) = rewatch_setup(
        "esc",
        json!({ "from": "review", "outcome": "changes_requested" }),
    )
    .await;
    let err = engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            SendMessageOrResumeError::ReplyIsOnlyMarkers { rewatch: false, .. }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains("choco task retry"));
    assert!(
        err.to_string()
            .contains(&format!("choco task cancel {task_id}"))
    );
}

#[tokio::test]
async fn a_marker_with_words_or_leading_space_is_a_note() {
    for text in ["/approve\nalso fix X", "  /approve"] {
        let (pool, _dir, task_id, engine, _now) =
            rewatch_setup("esc", json!({ "from": "review", "outcome": "timeout" })).await;
        engine.send_message_or_resume(&task_id, text).await.unwrap();
        assert_eq!(state_of(&pool, &task_id).await.current_stage, "revising");
        assert_eq!(human_messages(&pool, &task_id).await.len(), 1);
    }
}

#[tokio::test]
async fn a_workflow_without_marker_gates_takes_a_marker_like_reply_as_a_note() {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let yaml = "name: nomark\nstages:\n  gate:\n    kind: human_gate\n    on: { resumed: done }\n  done:\n    kind: terminal\n";
    std::fs::write(dir.join("nomark.yaml"), yaml).unwrap();
    let task_id = seed_task_in(&pool, "nomark", &dir).await;
    seed_row(&pool, &task_id, "gate", json!({ "task": { "title": "T" } })).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine
        .send_message_or_resume(&task_id, "/approve")
        .await
        .unwrap();
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "done");
}
