//! An omp background wake through the real engine, session manager and
//! `OmpAdapter`, driven by the scripted `fake_omp.py`: the accepted outcome
//! of the wake's own run is applied once, and nothing else is.

use super::*;
use crate::adapter::OmpAdapter;
use crate::db::{connect, events, projects, sessions, tasks};
use crate::session::TurnTimers;
use crate::test_support::OmpScript;
use chocofactory_core::models::SessionEndReason;

const WAKE_YAML: &str = "name: omp-wake\nroles:\n  reviewer:\n    cli: omp\n    model: openai-codex/gpt-5.6-terra\nstages:\n  review:\n    kind: agent_turn\n    role: reviewer\n    capture: json\n    on: { approved: finished, changes_requested: bounced }\n  finished:\n    kind: terminal\n  bounced:\n    kind: terminal\n";

/// The distinctive summary the wake reports.
const SUMMARY: &str = "wake-summary-7f3a: the retry loop drops the last element";

struct WakeRun {
    pool: SqlitePool,
    task_id: String,
    script: OmpScript,
    engine: Arc<WorkflowEngine>,
    _dir: TempDir,
    _repo: TempDir,
}

fn timers() -> TurnTimers {
    TurnTimers {
        grace: StdDuration::from_secs(5),
        nudge_after: StdDuration::from_secs(60),
        max_nudges: 3,
        job_wait_limit: StdDuration::from_secs(60),
    }
}

/// A task on `WAKE_YAML` in a disposable git repository, with a file-backed
/// database, whose reviewer is a real `OmpAdapter` on the scripted fixture.
async fn start_wake(steps: Value, timers: TurnTimers) -> WakeRun {
    let dir = tempdir();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let pool = connect(&dir.join("choco.db")).await.unwrap();
    let script = OmpScript::new(&dir, steps, &[]);
    let def = Arc::new(WorkflowDefinition::parse(WAKE_YAML, &dir).unwrap());
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
            closes: &[],
        },
    )
    .await
    .unwrap()
    .id;
    let adapter: Arc<dyn AgentAdapter> = Arc::new(OmpAdapter::with_binary(
        &script.binary,
        dir.join("omp-state"),
    ));
    let events_notify = Arc::new(Notify::new());
    let manager = SessionManager::with_turn_timers(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
        timers,
    );
    let engine = WorkflowEngine::new(
        pool.clone(),
        manager,
        PathBuf::from("."),
        None,
        events_notify,
    );
    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    WakeRun {
        pool,
        task_id,
        script,
        engine,
        _dir: dir,
        _repo: repo,
    }
}

impl WakeRun {
    async fn session(&self) -> Session {
        crate::test_support::wait_until("the review session", || async {
            sessions::list_for_task(&self.pool, &self.task_id)
                .await
                .unwrap()
                .into_iter()
                .next()
                .ok_or_else(|| "no session yet".to_string())
        })
        .await
    }

    async fn notes(&self) -> Vec<String> {
        let session = self.session().await;
        events::list_for_session(&self.pool, &session.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionNote)
            .map(|e| e.payload["kind"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    async fn outcomes(&self) -> Vec<Value> {
        events::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::TurnOutcome)
            .map(|e| e.payload)
            .collect()
    }

    /// Everything the run left: for failure messages.
    async fn describe(&self) -> String {
        let task = tasks::get(&self.pool, &self.task_id)
            .await
            .unwrap()
            .unwrap();
        let session = self.session().await;
        let all = events::list_for_session(&self.pool, &session.id)
            .await
            .unwrap();
        format!(
            "task {} stuck={:?}; session {:?}/{:?}; trail {:?}; events {:?}; fixture {:?}",
            task.status,
            task.stuck_reason,
            session.status,
            session.end_reason,
            stage_trail(&self.pool, &self.task_id).await,
            all.iter()
                .map(|e| format!("{}:{}", e.event_type, e.payload))
                .collect::<Vec<_>>(),
            self.script.lines()
        )
    }
}

/// The wake's steps. `awaiting` is the first end's `awaitingAsyncWork` and
/// `tail` what the wake does after reporting.
fn wake_steps(awaiting: Value, reported: Value, tail: Vec<Value>) -> Value {
    let mut steps = vec![
        json!({"op": "pid"}),
        json!({"op": "agent_start"}),
        json!({"op": "assistant", "text": "started the service"}),
        json!({"op": "agent_end", "terminal": false, "awaiting": awaiting}),
        json!({"op": "prompt_result", "settled": awaiting == json!("ABSENT")}),
        json!({"op": "gate", "name": "wake"}),
        json!({"op": "agent_start"}),
        json!({"op": "assistant", "text": "the service finished"}),
        json!({"op": "report", "args": reported}),
    ];
    steps.extend(tail);
    Value::Array(steps)
}

fn changes_requested() -> Value {
    json!({"outcome": "changes_requested", "summary": SUMMARY})
}

fn clean_finish() -> Vec<Value> {
    vec![
        json!({"op": "assistant", "text": "{\"outcome\": \"changes_requested\"}"}),
        json!({"op": "agent_end"}),
        json!({"op": "session_settled"}),
    ]
}

/// The tail of a failed wake: the final message fails and the run yields.
fn failing_finish(reason: &str) -> Vec<Value> {
    vec![
        json!({"op": "assistant", "text": "stopped", "stop_reason": reason,
               "error_message": "provider fell over mid-answer"}),
        json!({"op": "agent_end"}),
        json!({"op": "session_settled"}),
    ]
}

/// Report accepted, then the process goes away without a final end.
fn no_final_end() -> Vec<Value> {
    vec![
        json!({"op": "turn_end"}),
        json!({"op": "session_settled"}),
        json!({"op": "exit", "code": 0}),
    ]
}

async fn assert_applied_once(run: &WakeRun) {
    wait_until_stage(&run.pool, &run.task_id, "bounced").await;
    let outcome = wait_until_turn_outcome_event(&run.pool, &run.task_id).await;
    wait_until_task_status(&run.pool, &run.task_id, "closed").await;
    assert_eq!(outcome["outcome"], "changes_requested", "{outcome}");
    assert_eq!(outcome["source"], "tool", "{outcome}");
    assert_eq!(outcome["applied"], true, "{outcome}");
    // The captured report is what later stages see.
    let captured = payload_of(&run.pool, &run.task_id).await;
    assert_eq!(captured["stages"]["review"]["outcome"], "changes_requested");
    assert_eq!(captured["stages"]["review"]["summary"], SUMMARY);
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![
            ("review".to_string(), Value::Null),
            ("bounced".to_string(), json!("changes_requested")),
        ],
        "{}",
        run.describe().await
    );
    assert_eq!(run.outcomes().await.len(), 1, "one ordinary application");
    assert_eq!(
        run.script.prompts_received(),
        1,
        "no nudge or second prompt"
    );
    // The session closed stdin and the child went away.
    run.script.wait_log("eof").await;
    let kinds = run.notes().await;
    for unwanted in ["nudge", "no_report", "lingered"] {
        assert!(!kinds.iter().any(|k| k == unwanted), "{kinds:?}");
    }
}

/// A failed or unfinished wake must advance nothing: the task parks at the
/// review stage with no outcome written.
async fn assert_not_applied(run: &WakeRun, what: &str) {
    // Any settled state ends the wait, so an application that should not
    // have happened (a closed task) fails the assertions below, not a wait.
    let status = crate::test_support::wait_until("the task to settle", || async {
        let status = tasks::get(&run.pool, &run.task_id)
            .await
            .unwrap()
            .unwrap()
            .status;
        match status.as_str() {
            "stuck" | "closed" | "cancelled" => Ok(status),
            other => Err(other.to_string()),
        }
    })
    .await;
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![("review".to_string(), Value::Null)],
        "{what}: nothing may advance: {}",
        run.describe().await
    );
    assert!(
        run.outcomes().await.is_empty(),
        "{what}: no outcome applied: {}",
        run.describe().await
    );
    assert_eq!(status, "stuck", "{what}: {}", run.describe().await);
}

#[tokio::test]
async fn omp_wake_routes_the_accepted_outcome_once() {
    for awaiting in [json!(true), json!("ABSENT")] {
        let run = start_wake(
            wake_steps(awaiting.clone(), changes_requested(), clean_finish()),
            timers(),
        )
        .await;
        // The first end was report-less: the task is waiting in its stage.
        run.script.wait_log("op:gate").await;
        assert!(run.outcomes().await.is_empty());
        assert_eq!(
            tasks::get(&run.pool, &run.task_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        run.script.release("wake");
        assert_applied_once(&run).await;
    }
}

#[tokio::test]
async fn omp_a_failed_or_unfinished_wake_is_not_applied() {
    type Tail = fn() -> Vec<Value>;
    let cases: [(&str, Tail); 3] = [
        ("an errored wake", || failing_finish("error")),
        ("an aborted wake", || failing_finish("aborted")),
        ("a wake with no final end", no_final_end),
    ];
    for (what, tail) in cases {
        let run = start_wake(
            wake_steps(json!(true), changes_requested(), tail()),
            timers(),
        )
        .await;
        run.script.wait_log("op:gate").await;
        run.script.release("wake");
        assert_not_applied(&run, what).await;
    }
}

/// The accepted report is in the history but no clean end follows: the
/// session ends `(Exited, NoReport)` and the report alone is not applied.
#[tokio::test]
async fn omp_clean_eof_after_a_report_is_no_report() {
    let run = start_wake(
        wake_steps(json!(true), changes_requested(), no_final_end()),
        timers(),
    )
    .await;
    run.script.wait_log("op:gate").await;
    run.script.release("wake");
    assert_not_applied(&run, "eof").await;
    let session = run.session().await;
    assert_eq!(session.status, SessionStatus::Exited);
    assert_eq!(session.end_reason, Some(SessionEndReason::NoReport));
}

/// A named-service shape: nothing is signalled pending, the ticket is
/// settled, so the ordinary silence nudge runs; the wake still completes
/// the turn before the nudges are used up.
#[tokio::test]
async fn omp_unsignalled_work_is_nudged_and_a_wake_before_exhaustion_applies_once() {
    let mut tail = clean_finish();
    // The nudge's queued follow-up runs after the wake: late events must not
    // re-apply anything.
    tail.extend([
        json!({"op": "agent_start"}),
        json!({"op": "assistant", "text": "answering the nudge"}),
        json!({"op": "agent_end"}),
    ]);
    let timers = TurnTimers {
        nudge_after: StdDuration::from_millis(300),
        max_nudges: 4,
        ..timers()
    };
    let run = start_wake(
        wake_steps(json!("ABSENT"), changes_requested(), tail),
        timers,
    )
    .await;
    run.script.wait_log("op:gate").await;
    // The ordinary silence nudge is actually sent while nothing is reported.
    run.script.wait_prompts(2).await;
    assert!(run.outcomes().await.is_empty());
    assert!(run.notes().await.iter().any(|k| k == "nudge"));
    run.script.release("wake");
    wait_until_stage(&run.pool, &run.task_id, "bounced").await;
    wait_until_task_status(&run.pool, &run.task_id, "closed").await;
    run.script.wait_log("script_done").await;
    run.script.wait_log("eof").await;
    assert_eq!(run.outcomes().await.len(), 1);
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![
            ("review".to_string(), Value::Null),
            ("bounced".to_string(), json!("changes_requested")),
        ]
    );
}

#[tokio::test]
async fn omp_unsignalled_work_that_fails_or_never_ends_is_still_not_applied() {
    type Tail = fn() -> Vec<Value>;
    let cases: [(&str, Tail); 3] = [
        ("an errored wake", || failing_finish("error")),
        ("an aborted wake", || failing_finish("aborted")),
        ("a wake with no final end", no_final_end),
    ];
    for (what, tail) in cases {
        let timers = TurnTimers {
            nudge_after: StdDuration::from_millis(300),
            max_nudges: 4,
            ..timers()
        };
        let run = start_wake(
            wake_steps(json!("ABSENT"), changes_requested(), tail()),
            timers,
        )
        .await;
        run.script.wait_log("op:gate").await;
        run.script.wait_prompts(2).await;
        run.script.release("wake");
        assert_not_applied(&run, what).await;
    }
}

/// Nudges used up before any wake: the task is parked and a later wake
/// cannot revive it (the session has closed the process).
#[tokio::test]
async fn omp_a_wake_after_nudges_run_out_does_not_revive_the_turn() {
    let timers = TurnTimers {
        nudge_after: StdDuration::from_millis(200),
        max_nudges: 1,
        grace: StdDuration::from_secs(2),
        ..timers()
    };
    let run = start_wake(
        wake_steps(json!("ABSENT"), changes_requested(), clean_finish()),
        timers,
    )
    .await;
    run.script.wait_log("op:gate").await;
    wait_until_task_status(&run.pool, &run.task_id, "stuck").await;
    run.script.release("wake");
    assert_not_applied(&run, "a wake after exhaustion").await;
    let session = run.session().await;
    assert_eq!(session.end_reason, Some(SessionEndReason::NoReport));
}

#[tokio::test]
async fn omp_cancel_during_background_wait_does_not_apply() {
    let run = start_wake(
        wake_steps(json!(true), changes_requested(), clean_finish()),
        timers(),
    )
    .await;
    run.script.wait_log("op:gate").await;
    // The turn is waiting on signalled pending work.
    crate::test_support::wait_until("the wait note", || async {
        if run.notes().await.iter().any(|k| k == "job_wait") {
            Ok(())
        } else {
            Err("no job_wait note".to_string())
        }
    })
    .await;
    let pid = run.script.pid();
    assert!(process_alive(pid), "the fixture is the waiting child");

    run.engine.cancel_task(&run.task_id, false).await.unwrap();
    // Releasing it afterwards must not matter.
    run.script.release("wake");

    let task = tasks::get(&run.pool, &run.task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "cancelled");
    let session = run.session().await;
    crate::test_support::wait_until("the session to end as cancelled", || async {
        let s = sessions::get(&run.pool, &session.id)
            .await
            .unwrap()
            .unwrap();
        if s.status == SessionStatus::Active {
            Err("still active".to_string())
        } else {
            Ok(s)
        }
    })
    .await;
    let session = sessions::get(&run.pool, &session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.end_reason, Some(SessionEndReason::Cancelled));
    assert!(run.outcomes().await.is_empty(), "{}", run.describe().await);
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![("review".to_string(), Value::Null)]
    );
    crate::test_support::wait_until("the child to be reaped", || async {
        if process_alive(pid) {
            Err(format!("pid {pid} still alive"))
        } else {
            Ok(())
        }
    })
    .await;
}
